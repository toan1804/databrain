//! Model Context Protocol server (JSON-RPC 2.0, newline-delimited over
//! stdio). Lets external agents such as Kiro CLI or Claude Code use
//! DataBrain's tools on connections that opted in (`ai_policy.mcp_enabled`).
//! The same policy engine applies; anything that would need an approval
//! prompt is refused, so only auto-approved reads run.

use std::sync::Arc;

use databrain_query_engine::{EventHub, QueryEngine};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::tools::{Caller, ToolContext, ToolHost, specs};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

struct NoHost;

#[async_trait::async_trait]
impl ToolHost for NoHost {
    async fn approve(&self, _: &str, _: &str, _: &Value) -> Option<Value> {
        None
    }
}

pub struct McpServer {
    pub engine: Arc<QueryEngine>,
    pub hub: Arc<EventHub>,
}

fn tool_list() -> Vec<Value> {
    let mut out = vec![json!({
        "name": "list_connections",
        "description": "List DataBrain database connections available to this agent (name, type, environment).",
        "inputSchema": {"type": "object", "properties": {}},
    })];
    for t in specs(false) {
        // `save_query` and notes are allowed; every tool gets a `connection` argument.
        let mut schema = t.parameters.clone();
        schema["properties"]["connection"] = json!({"type": "string", "description": "Connection name or id (see list_connections)"});
        let mut req: Vec<Value> = schema["required"].as_array().cloned().unwrap_or_default();
        req.insert(0, json!("connection"));
        schema["required"] = Value::Array(req);
        out.push(json!({"name": t.name, "description": t.description, "inputSchema": schema}));
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

    async fn call_tool(&self, name: &str, args: &Value) -> (String, bool) {
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
            host: Arc::new(NoHost),
            cancel: Default::default(),
            results: parking_lot::Mutex::new(vec![]),
        };
        let o = ctx.call(name, args).await;
        let is_err = o.content.starts_with("ERROR");
        (o.content, is_err)
    }

    /// Handle one JSON-RPC message. Returns the response (None for notifications).
    pub async fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or("");
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg.pointer("/params/protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSION),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "databrain", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Use list_connections first. search_schema/describe_table explain the database; run_query executes read-only SQL where the connection allows it; query_result aggregates results locally.",
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_list()})),
            "tools/call" => {
                let name = msg.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
                let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = self.call_tool(name, &args).await;
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

    /// Serve until EOF.
    pub async fn serve<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(&self, reader: R, mut writer: W) -> std::io::Result<()> {
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let resp = match serde_json::from_str::<Value>(&line) {
                Ok(Value::Array(batch)) => {
                    let mut out = Vec::new();
                    for m in &batch {
                        if let Some(r) = self.handle(m).await {
                            out.push(r);
                        }
                    }
                    (!out.is_empty()).then_some(Value::Array(out))
                }
                Ok(m) => self.handle(&m).await,
                Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})),
            };
            if let Some(r) = resp {
                writer.write_all(serde_json::to_string(&r).unwrap_or_default().as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
        }
        Ok(())
    }
}
