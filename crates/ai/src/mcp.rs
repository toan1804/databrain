//! Model Context Protocol server (JSON-RPC 2.0, newline-delimited over
//! stdio). Lets external agents such as Kiro CLI or Claude Code use
//! DataBrain's tools on connections that opted in (`ai_policy.mcp_enabled`).
//! The same policy engine applies. What the connection's policy forbids is
//! refused. What DataBrain would ask the user about is asked through the
//! client (MCP elicitation) when the client supports it; otherwise it is
//! refused, unless the connection accepts the client's own tool approval
//! (`ai_policy.mcp_client_approves`): many clients approve tools on their own.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use databrain_query_engine::{EventHub, QueryEngine};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::tools::{Caller, ToolContext, ToolHost, specs};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long DataBrain waits for the user to answer an elicitation.
pub const ELICIT_TIMEOUT: Duration = Duration::from_secs(300);

/// The client on the other end: requests DataBrain sends it (elicitation)
/// and their pending answers.
#[derive(Default)]
struct Peer {
    out: Option<mpsc::UnboundedSender<Value>>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    next_id: AtomicI64,
    /// The client declared the `elicitation` capability in `initialize`.
    elicitation: AtomicBool,
    /// The input ended: nobody will answer.
    closed: AtomicBool,
}

impl Peer {
    /// Send a request to the client and wait for its answer (None: no answer,
    /// the connection closed or it timed out).
    async fn request(&self, method: &str, params: Value) -> Option<Value> {
        let out = self.out.as_ref()?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        if self.closed.load(Ordering::SeqCst) || out.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).is_err() {
            self.pending.lock().remove(&id);
            return None;
        }
        let answer = tokio::time::timeout(ELICIT_TIMEOUT, rx).await;
        self.pending.lock().remove(&id);
        answer.ok()?.ok()
    }

    /// An answer from the client to one of our requests.
    fn resolve(&self, msg: &Value) {
        if let Some(tx) = msg["id"].as_i64().and_then(|id| self.pending.lock().remove(&id)) {
            let _ = tx.send(msg.clone());
        }
    }
}

/// Approvals over MCP: DataBrain's question, shown to the user by the client.
struct McpHost {
    peer: Arc<Peer>,
}

#[async_trait::async_trait]
impl ToolHost for McpHost {
    fn can_ask(&self) -> bool {
        self.peer.out.is_some() && self.peer.elicitation.load(Ordering::Relaxed)
    }

    async fn approve(&self, tool: &str, summary: &str, detail: &Value) -> Option<Value> {
        if !self.can_ask() {
            return None;
        }
        let mut message = format!("DataBrain asks before running {tool}: {summary}");
        if let Some(sql) = detail.get("sql").and_then(|v| v.as_str()) {
            message.push_str(&format!("\n\n{sql}"));
        }
        let params = json!({
            "message": message,
            "requestedSchema": {
                "type": "object",
                "properties": {"approve": {"type": "boolean", "title": "Approve", "description": "Run it", "default": false}},
                "required": ["approve"],
            },
        });
        let answer = self.peer.request("elicitation/create", params).await?;
        let r = &answer["result"];
        // Only an explicit accept with approve = true runs it.
        (r["action"] == "accept" && r["content"]["approve"] == json!(true)).then(|| detail.clone())
    }
}

pub struct McpServer {
    pub engine: Arc<QueryEngine>,
    pub hub: Arc<EventHub>,
}

/// MCP tool annotations: what a tool does, so clients can auto-approve the
/// safe ones. `(read_only, destructive, idempotent)`; nothing reaches outside
/// the user's own connections and DataBrain workspace.
fn annotations(name: &str) -> Value {
    let (read_only, destructive, idempotent) = match name {
        "list_connections" | "search_schema" | "describe_table" | "list_tables" | "get_sample_rows" | "query_result" | "result_summary"
        | "list_outputs" | "query_outputs" | "list_knowledge_notes" | "list_jobs" | "get_job" => (true, false, true),
        // Refreshes DataBrain's own index of table descriptions.
        "index_tables" => (false, false, true),
        "add_knowledge_note" | "save_query" | "create_job" => (false, false, false),
        // Replaces a note / a job's steps.
        "update_knowledge_note" | "update_job" => (false, true, true),
        // SQL (writes where the connection's AI policy allows them) and jobs
        // (load steps write into tables).
        _ => (false, true, false),
    };
    json!({"readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": idempotent, "openWorldHint": false})
}

fn tool_list() -> Vec<Value> {
    let mut out = vec![json!({
        "name": "list_connections",
        "description": "List DataBrain database connections available to this agent (name, type, environment).",
        "inputSchema": {"type": "object", "properties": {}},
        "annotations": annotations("list_connections"),
    })];
    for t in specs(false) {
        // `save_query` and notes are allowed; every tool gets a `connection` argument.
        let mut schema = t.parameters.clone();
        schema["properties"]["connection"] = json!({"type": "string", "description": "Connection name or id (see list_connections)"});
        let mut req: Vec<Value> = schema["required"].as_array().cloned().unwrap_or_default();
        req.insert(0, json!("connection"));
        schema["required"] = Value::Array(req);
        out.push(json!({"name": t.name, "description": t.description, "inputSchema": schema, "annotations": annotations(&t.name)}));
    }
    out
}

impl McpServer {
    fn connections(&self) -> Vec<databrain_workspace::ConnectionProfile> {
        self.engine
            .workspace()
            .list_connections()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.ai_policy.mcp_enabled && c.ai_policy.ai_enabled)
            .collect()
    }

    async fn call_tool(&self, name: &str, args: &Value, peer: &Arc<Peer>) -> (String, bool) {
        if name == "list_connections" {
            let list: Vec<Value> = self
                .connections()
                .iter()
                .map(|c| json!({"name": c.name, "id": c.id, "type": c.config.kind.dialect_name(), "environment": c.env, "run_query": c.ai_policy.run_query}))
                .collect();
            return if list.is_empty() {
                ("No connections are shared with MCP. In DataBrain, open a connection's AI settings and enable \"Allow external agents (MCP)\".".into(), false)
            } else {
                (serde_json::to_string_pretty(&list).unwrap_or_default(), false)
            };
        }
        let Some(conn) = args.get("connection").and_then(|c| c.as_str()) else {
            return ("missing `connection` argument".into(), true);
        };
        let Some(profile) = self.connections().into_iter().find(|c| c.id == conn || c.name.eq_ignore_ascii_case(conn)) else {
            return (format!("connection `{conn}` not found or not shared with MCP"), true);
        };
        let ctx = ToolContext {
            engine: self.engine.clone(),
            hub: self.hub.clone(),
            profile,
            session_id: None,
            caller: Caller::Mcp,
            host: Arc::new(McpHost { peer: peer.clone() }),
            cancel: Default::default(),
            results: parking_lot::Mutex::new(vec![]),
        };
        let o = ctx.call(name, args).await;
        let is_err = o.content.starts_with("ERROR");
        (o.content, is_err)
    }

    /// Handle one JSON-RPC message without a way to reach the client (no
    /// elicitation). Returns the response (None for notifications).
    pub async fn handle(&self, msg: &Value) -> Option<Value> {
        self.handle_with(msg, &Arc::new(Peer::default())).await
    }

    async fn handle_with(&self, msg: &Value, peer: &Arc<Peer>) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or("");
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                let elicitation = msg.pointer("/params/capabilities/elicitation").is_some_and(|c| !c.is_null());
                peer.elicitation.store(elicitation, Ordering::Relaxed);
                Ok(json!({
                "protocolVersion": msg.pointer("/params/protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSION),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "databrain", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Use list_connections first. search_schema/describe_table explain the database; run_query executes read-only SQL where the connection allows it; query_result aggregates results locally.",
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_list()})),
            "tools/call" => {
                let name = msg.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
                let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = self.call_tool(name, &args, peer).await;
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
            m if m.starts_with("notifications/") => return None,
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        let id = id?;
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        })
    }

    /// Serve until EOF. Requests are handled concurrently, so a tool call
    /// waiting for the user's answer (elicitation) doesn't hold up the
    /// client's other messages, including that answer.
    pub async fn serve<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(&self, reader: R, mut writer: W) -> std::io::Result<()> {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
        let peer = Arc::new(Peer { out: Some(out_tx), ..Default::default() });
        let mut lines = reader.lines();
        let mut running = FuturesUnordered::new();
        let mut eof = false;
        while !(eof && running.is_empty()) {
            tokio::select! {
                line = lines.next_line(), if !eof => {
                    let Some(line) = line? else {
                        eof = true;
                        // Nobody can answer now: waiting approvals are declined.
                        peer.closed.store(true, Ordering::SeqCst);
                        peer.pending.lock().clear();
                        continue;
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(&line) {
                        // An answer to one of our requests.
                        Ok(m) if m.get("method").is_none() && m.get("id").is_some() => peer.resolve(&m),
                        Ok(m) => {
                            let peer = peer.clone();
                            running.push(async move {
                                match m {
                                    Value::Array(batch) => {
                                        let mut out = Vec::new();
                                        for m in &batch {
                                            if m.get("method").is_none() && m.get("id").is_some() {
                                                peer.resolve(m);
                                                continue;
                                            }
                                            if let Some(r) = self.handle_with(m, &peer).await {
                                                out.push(r);
                                            }
                                        }
                                        (!out.is_empty()).then_some(Value::Array(out))
                                    }
                                    m => self.handle_with(&m, &peer).await,
                                }
                            });
                        }
                        Err(e) => write_line(&mut writer, &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})).await?,
                    }
                }
                Some(resp) = running.next(), if !running.is_empty() => {
                    if let Some(r) = resp {
                        write_line(&mut writer, &r).await?;
                    }
                }
                Some(req) = out_rx.recv() => write_line(&mut writer, &req).await?,
            }
        }
        Ok(())
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, v: &Value) -> std::io::Result<()> {
    writer.write_all(serde_json::to_string(v).unwrap_or_default().as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}
