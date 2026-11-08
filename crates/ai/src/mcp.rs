//! Model Context Protocol server (JSON-RPC 2.0, newline-delimited over
//! stdio). Lets external agents such as Kiro CLI or Claude Code use
//! DataBrain's tools on connections that opted in (`ai_policy.mcp_enabled`).
//! The same policy engine applies. What the connection's policy forbids is
//! refused. What DataBrain would ask the user about runs on the client's own
//! tool approval where the connection accepts it
//! (`ai_policy.mcp_client_approves`); otherwise it is asked through the
//! client (MCP elicitation) when the client supports it, and refused when it
//! doesn't, with a guide for the user to turn that setting on.
//! A question the client never answers (it declared elicitation but doesn't
//! show prompts in its mode) times out with a message that says what to
//! change, and no further question is asked in that session. Jobs are
//! created, changed and run only in DataBrain's assistant
//! (`tools::APP_ONLY_TOOLS`); MCP clients can list and read them.

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

use crate::tools::{APP_ONLY_TOOLS, Caller, ToolContext, ToolHost, specs};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Default wait for the user's answer to an elicitation, in seconds
/// (`DATABRAIN_MCP_ELICIT_SECS` overrides it, 5 s to 1 h).
pub const ELICIT_SECS: u64 = 120;

/// Set by [`set_elicit_secs`] (tests); 0 = not set.
static ELICIT_OVERRIDE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Override the elicitation wait for this process (tests).
#[doc(hidden)]
pub fn set_elicit_secs(secs: u64) {
    ELICIT_OVERRIDE.store(secs, Ordering::SeqCst);
}

/// How long DataBrain waits for the user to answer an elicitation.
pub fn elicit_timeout() -> Duration {
    let secs = match ELICIT_OVERRIDE.load(Ordering::SeqCst) {
        0 => std::env::var("DATABRAIN_MCP_ELICIT_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(ELICIT_SECS),
        n => n,
    };
    Duration::from_secs(secs.clamp(5, 3600))
}

/// Why a request to the client got no answer.
enum NoAnswer {
    /// The client declared elicitation but didn't answer in time: it may
    /// not show prompts in this mode. Not asked again this session.
    Timeout,
    /// No way to reach the client, or it closed.
    Gone,
}

/// The client on the other end: requests DataBrain sends it (elicitation)
/// and their pending answers.
#[derive(Default)]
struct Peer {
    out: Option<mpsc::UnboundedSender<Value>>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    next_id: AtomicI64,
    /// The client declared the `elicitation` capability in `initialize`.
    elicitation: AtomicBool,
    /// A question timed out: the client doesn't show them (or nobody is
    /// there). Later questions are refused at once instead of waiting.
    unanswered: AtomicBool,
    /// The input ended: nobody will answer.
    closed: AtomicBool,
    /// Tool calls in progress by request id (JSON text): the client's
    /// `notifications/cancelled` stops them.
    calls: Mutex<HashMap<String, tokio_util::sync::CancellationToken>>,
}

impl Peer {
    /// Send a request to the client and wait for its answer.
    async fn request(&self, method: &str, params: Value) -> std::result::Result<Value, NoAnswer> {
        let out = self.out.as_ref().ok_or(NoAnswer::Gone)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        if self.closed.load(Ordering::SeqCst) || out.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).is_err() {
            self.pending.lock().remove(&id);
            return Err(NoAnswer::Gone);
        }
        let answer = tokio::time::timeout(elicit_timeout(), rx).await;
        self.pending.lock().remove(&id);
        match answer {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(NoAnswer::Gone),
            Err(_) => {
                self.unanswered.store(true, Ordering::SeqCst);
                Err(NoAnswer::Timeout)
            }
        }
    }

    /// The client gave up on request `id`: stop its tool call (its query,
    /// or the question it waits on).
    fn cancel_call(&self, id: &Value) {
        if let Some(t) = self.calls.lock().get(&id.to_string()) {
            t.cancel();
        }
    }

    /// Send a notification to the client (nothing to wait for).
    fn notify(&self, method: &str, params: Value) {
        if let Some(out) = &self.out {
            let _ = out.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
        }
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
    /// The call's `_meta.progressToken`: while the question waits, the client
    /// is told so (`notifications/progress`) instead of showing a bare spinner.
    progress: Option<Value>,
    /// Fires when the client cancels the call: a question it waits on ends.
    cancel: tokio_util::sync::CancellationToken,
}

#[async_trait::async_trait]
impl ToolHost for McpHost {
    fn can_ask(&self) -> bool {
        self.peer.out.is_some()
            && self.peer.elicitation.load(Ordering::Relaxed)
            && !self.peer.unanswered.load(Ordering::SeqCst)
            && !self.peer.closed.load(Ordering::SeqCst)
    }

    fn cannot_ask_reason(&self) -> String {
        if self.peer.out.is_none() || !self.peer.elicitation.load(Ordering::Relaxed) {
            "this MCP client can't show DataBrain's question (it doesn't support elicitation)".into()
        } else if self.peer.closed.load(Ordering::SeqCst) {
            "the MCP client closed the connection".into()
        } else {
            format!(
                "this MCP client didn't answer DataBrain's question within {} s (it may not show elicitation prompts in this mode; no further questions are asked in this session)",
                elicit_timeout().as_secs()
            )
        }
    }

    async fn approve(&self, tool: &str, summary: &str, detail: &Value) -> Option<Value> {
        if !self.can_ask() {
            return None;
        }
        let mut message = format!("DataBrain asks before running {tool}: {summary}");
        if let Some(sql) = detail.get("sql").and_then(|v| v.as_str()) {
            message.push_str(&format!("\n\n{sql}"));
        }
        if let Some(token) = &self.progress {
            self.peer.notify(
                "notifications/progress",
                json!({"progressToken": token, "progress": 1, "message": format!("Waiting for your answer to DataBrain's question (approve {tool}?) in this client")}),
            );
        }
        let params = json!({
            "message": message,
            "requestedSchema": {
                "type": "object",
                "properties": {"approve": {"type": "boolean", "title": "Approve", "description": "Run it", "default": false}},
                "required": ["approve"],
            },
        });
        let asked = tokio::select! {
            a = self.peer.request("elicitation/create", params) => a,
            _ = self.cancel.cancelled() => return None,
        };
        let answer = match asked {
            Ok(a) => a,
            Err(NoAnswer::Timeout) => {
                tracing::warn!(
                    tool,
                    secs = elicit_timeout().as_secs(),
                    "no answer to the elicitation: the client may not show prompts; not asking again this session"
                );
                return None;
            }
            Err(NoAnswer::Gone) => return None,
        };
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
        "list_connections"
        | "search_schema"
        | "describe_table"
        | "find_join_path"
        | "list_tables"
        | "get_sample_rows"
        | "query_result"
        | "result_summary"
        | "list_outputs"
        | "query_outputs"
        | "list_knowledge_notes"
        | "list_jobs"
        | "get_job" => (true, false, true),
        // Refreshes DataBrain's own index of table descriptions.
        "index_tables" => (false, false, true),
        "add_knowledge_note" | "save_query" => (false, false, false),
        // Recording the same join again changes nothing but its source.
        "add_knowledge_join" => (false, false, true),
        // Replaces a note's text.
        "update_knowledge_note" => (false, true, true),
        // SQL (writes where the connection's AI policy allows them).
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
    // Creating, changing and running jobs stays in DataBrain's assistant.
    for t in specs(false).into_iter().filter(|t| !APP_ONLY_TOOLS.contains(&t.name.as_str())) {
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
        self.engine.workspace().list_connections().unwrap_or_default().into_iter().filter(|c| c.ai_policy.mcp_enabled && c.ai_policy.ai_enabled).collect()
    }

    async fn call_tool(
        &self,
        name: &str,
        args: &Value,
        peer: &Arc<Peer>,
        progress: Option<Value>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> (String, bool) {
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
            host: Arc::new(McpHost { peer: peer.clone(), progress, cancel: cancel.clone() }),
            cancel,
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
                "instructions": "Use list_connections first. search_schema/describe_table explain the database; run_query executes SQL where the connection allows it; query_result aggregates results locally. list_jobs/get_job read jobs; creating, changing and running jobs is only available in DataBrain's own assistant. When a call is refused because DataBrain needs the user's approval, pass the error's guide on to the user.",
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_list()})),
            "tools/call" => {
                let name = msg.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
                let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                let progress = msg.pointer("/params/_meta/progressToken").cloned().filter(|t| !t.is_null());
                let cancel = tokio_util::sync::CancellationToken::new();
                let key = id.as_ref().map(Value::to_string);
                if let Some(k) = &key {
                    peer.calls.lock().insert(k.clone(), cancel.clone());
                }
                let (text, is_error) = self.call_tool(name, &args, peer, progress, cancel.clone()).await;
                if let Some(k) = &key {
                    peer.calls.lock().remove(k);
                }
                // Cancelled by the client: it ignores any answer, so none is sent.
                if cancel.is_cancelled() {
                    return None;
                }
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
            "notifications/cancelled" => {
                if let Some(rid) = msg.pointer("/params/requestId") {
                    peer.cancel_call(rid);
                }
                return None;
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
        let mut lines = BoundedLines::new(reader, MAX_LINE_BYTES);
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
                    let line = match line {
                        Ok(l) => l,
                        Err(why) => {
                            write_line(&mut writer, &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": why}})).await?;
                            continue;
                        }
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

/// Longest message accepted from the client; a longer line is skipped
/// (answered with an error) without being kept in memory.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Newline-separated messages of at most `max` bytes. `next` is cancel-safe
/// (it only waits in `fill_buf`), so it can be used in `select!`.
struct BoundedLines<R> {
    reader: R,
    max: usize,
    buf: Vec<u8>,
    /// In a line that went over `max`: drop bytes until its end.
    skipping: bool,
    eof: bool,
}

impl<R: AsyncBufRead + Unpin> BoundedLines<R> {
    fn new(reader: R, max: usize) -> Self {
        Self { reader, max, buf: Vec::new(), skipping: false, eof: false }
    }

    /// The next line (without `\n` / `\r\n`), `Err(reason)` for one that is
    /// too long or not UTF-8, `None` at the end.
    async fn next_line(&mut self) -> std::io::Result<Option<Result<String, String>>> {
        loop {
            if self.eof {
                return Ok(None);
            }
            let avail = self.reader.fill_buf().await?;
            let (chunk, end, used) = match avail.iter().position(|&b| b == b'\n') {
                Some(i) => (&avail[..i], true, i + 1),
                None if avail.is_empty() => {
                    self.eof = true;
                    if self.buf.is_empty() && !self.skipping {
                        return Ok(None);
                    }
                    (&avail[..0], true, 0)
                }
                None => (avail, false, avail.len()),
            };
            if !self.skipping {
                if self.buf.len() + chunk.len() > self.max {
                    self.skipping = true;
                    self.buf = Vec::new();
                } else {
                    self.buf.extend_from_slice(chunk);
                }
            }
            self.reader.consume(used);
            if end {
                if std::mem::take(&mut self.skipping) {
                    return Ok(Some(Err(format!("message longer than {} bytes", self.max))));
                }
                let mut line = std::mem::take(&mut self.buf);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8(line).map_err(|_| "message is not UTF-8".to_string())));
            }
        }
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, v: &Value) -> std::io::Result<()> {
    writer.write_all(serde_json::to_string(v).unwrap_or_default().as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

#[cfg(test)]
mod line_tests {
    use super::*;

    async fn all(input: &[u8], max: usize, cap: usize) -> Vec<Result<String, String>> {
        let mut l = BoundedLines::new(tokio::io::BufReader::with_capacity(cap, input), max);
        let mut out = vec![];
        while let Some(x) = l.next_line().await.unwrap() {
            out.push(x);
        }
        out
    }

    #[tokio::test]
    async fn caps_line_length() {
        for cap in [1, 3, 64] {
            let got = all(b"ab\r\n0123456789xyz\n\nlast", 8, cap).await;
            assert_eq!(got.len(), 4, "cap {cap}: {got:?}");
            assert_eq!(got[0], Ok("ab".into()));
            assert!(got[1].as_ref().unwrap_err().contains("longer than 8"), "cap {cap}");
            assert_eq!(got[2], Ok(String::new()));
            assert_eq!(got[3], Ok("last".into()), "a last line without newline");
        }
        assert!(all(b"0123456789", 8, 4).await[0].is_err(), "too long up to the end");
        assert!(all(b"\xff\xfe\n", 8, 4).await[0].as_ref().unwrap_err().contains("UTF-8"));
        assert!(all(b"", 8, 4).await.is_empty());
    }
}
