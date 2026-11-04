//! Tools the AI can call. Each tool is described by a JSON Schema (sent to
//! the model and exposed over MCP) and executed here with policy checks.

mod gate;
mod jobs;
mod notes;
mod query;
mod schema;
mod specs;
mod ui;

pub use specs::specs;

use std::sync::Arc;

use databrain_query_engine::{EventHub, QueryEngine};
use databrain_workspace::ConnectionProfile;
use serde_json::{Value, json};

use crate::types::{AiError, Result};

/// Where a tool call originated (for approvals and audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// In-app agent: `Ask` decisions go to the approval UI.
    Agent,
    /// External MCP client: `Ask` decisions are denied unless auto-approved.
    Mcp,
    /// In-app agent answering without the database (a connection-settings
    /// question): no tool may connect, so nothing signs in.
    AgentNoDatabase,
}

/// What to do when an MCP client can't carry DataBrain's question.
const MCP_ASK_HELP: &str = "Run it from DataBrain's assistant, set \"AI may run queries\" to automatic for read-only queries, or turn on \"Accept the MCP client's approval\" in the connection's AI settings";

/// Hooks into the host for tools that need the UI.
#[async_trait::async_trait]
pub trait ToolHost: Send + Sync {
    /// Ask the user to approve an action. Returns the (possibly edited)
    /// argument value on approval, `None` on denial.
    async fn approve(&self, tool: &str, summary: &str, detail: &Value) -> Option<Value>;
    /// Whether `approve` can reach the user. The MCP server can only when the
    /// client supports elicitation (and answered so far); otherwise an Ask is
    /// refused unless the connection accepts the client's own tool approval.
    fn can_ask(&self) -> bool {
        true
    }
    /// Why `approve` can't reach the user (when `can_ask` is false), as a
    /// clause for the refusal message.
    fn cannot_ask_reason(&self) -> String {
        "this MCP client can't show DataBrain's question (it doesn't support elicitation)".into()
    }
    /// Current editor state for `get_editor`.
    async fn editor_state(&self) -> Option<Value> {
        None
    }
    /// Apply an editor change proposal (`write_editor`). Returns whether the
    /// user accepted it.
    async fn propose_edit(&self, _proposal: &Value) -> bool {
        false
    }
    /// Result currently shown in the grid.
    fn current_result(&self) -> Option<String> {
        None
    }
    /// Start a job run (`only`: those step ids). Returns the run id.
    /// Hosts without a job runner (MCP, tests) can't.
    async fn start_job(&self, _job_id: &str, _only: Option<Vec<String>>) -> std::result::Result<i64, String> {
        Err("running jobs is only available in the DataBrain app".into())
    }
}

pub struct ToolContext {
    pub engine: Arc<QueryEngine>,
    pub hub: Arc<EventHub>,
    pub profile: ConnectionProfile,
    pub session_id: Option<String>,
    pub caller: Caller,
    pub host: Arc<dyn ToolHost>,
    pub cancel: tokio_util::sync::CancellationToken,
    /// Results produced by this session (allowed for query_result).
    pub results: parking_lot::Mutex<Vec<String>>,
}

pub struct ToolOutput {
    /// Sent back to the model.
    pub content: String,
    /// Shown in the UI (structured).
    pub display: Value,
}

fn out(content: impl Into<String>, display: Value) -> ToolOutput {
    ToolOutput { content: content.into(), display }
}

fn arg<'a>(args: &'a Value, k: &str) -> Result<&'a str> {
    args.get(k).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).ok_or_else(|| AiError::Policy(format!("missing argument `{k}`")))
}

impl ToolContext {
    fn audit(&self, tool: &str, args: &Value, decision: &str, summary: Option<&str>) {
        tracing::info!(tool, decision, caller = ?self.caller, connection = %self.profile.id, "AI tool call");
        let _ = self.engine.workspace().add_audit(self.session_id.as_deref(), Some(&self.profile.id), tool, args, decision, summary);
    }

    /// Execute one tool call. Errors are returned to the model as text.
    pub async fn call(&self, name: &str, args: &Value) -> ToolOutput {
        if self.caller == Caller::AgentNoDatabase {
            return out("ERROR: tools are off for this question (it is about connection settings, not data): answer from the text you were given", Value::Null);
        }
        let r = match name {
            "search_schema" => self.search_schema(args),
            "describe_table" => self.describe_table(args),
            "index_tables" => self.index_tables(args).await,
            "list_tables" => self.list_tables(args),
            "get_sample_rows" => self.sample_rows(args).await,
            "run_query" => self.run_query(args).await,
            "query_result" => self.query_result(args),
            "result_summary" => self.result_summary(args),
            "list_outputs" => self.list_outputs(args),
            "query_outputs" => self.query_outputs(args).await,
            "add_knowledge_note" => self.add_note(args).await,
            "update_knowledge_note" => self.update_note(args).await,
            "list_knowledge_notes" => self.list_notes(args),
            "save_query" => self.save_query(args).await,
            "list_connections" => self.list_connections(),
            "list_jobs" => self.list_jobs(args),
            "get_job" => self.get_job(args),
            "create_job" => self.create_job(args).await,
            "update_job" => self.update_job(args).await,
            "run_job" => self.run_job(args).await,
            "get_editor" => match self.host.editor_state().await {
                Some(v) => Ok(out(v.to_string(), v)),
                None => Err(AiError::Policy("no editor available".into())),
            },
            "write_editor" => self.write_editor(args).await,
            other => Err(AiError::Policy(format!("unknown tool `{other}`"))),
        };
        match r {
            Ok(o) => o,
            Err(e) => {
                let decision = if matches!(e, AiError::Policy(_)) { "blocked" } else { "error" };
                self.audit(name, args, decision, Some(&e.to_string()));
                out(format!("ERROR: {e}"), json!({"error": e.to_string()}))
            }
        }
    }

    /// Connection a schema tool looks at: `connection` (name or id) or the current one.
    fn schema_target(&self, args: &Value) -> Result<ConnectionProfile> {
        let Some(want) = args.get("connection").and_then(|c| c.as_str()).map(str::trim).filter(|c| !c.is_empty()) else {
            return Ok(self.profile.clone());
        };
        if want == self.profile.id || want.eq_ignore_ascii_case(&self.profile.name) {
            return Ok(self.profile.clone());
        }
        let refs = self.conn_refs()?;
        let c = crate::jobs::resolve_conn(&refs, want)?;
        let p = self.engine.workspace().get_connection(&c.id)?;
        // MCP hosts only see connections shared with them.
        if self.caller == Caller::Mcp && !p.ai_policy.mcp_enabled && !c.results {
            return Err(AiError::Policy(format!("\"{}\" is not shared with MCP", p.name)));
        }
        Ok(p)
    }

    /// Connections for jobs: every saved one, plus the Results (DuckDB) connection.
    fn conn_refs(&self) -> Result<Vec<crate::jobs::ConnRef>> {
        let results = self.engine.results_connection().ok().map(|p| p.id);
        Ok(self
            .engine
            .workspace()
            .list_connections()?
            .into_iter()
            .map(|p| crate::jobs::ConnRef {
                results: results.as_deref() == Some(p.id.as_str()),
                ai_enabled: p.ai_policy.ai_enabled || results.as_deref() == Some(p.id.as_str()),
                read_only: p.config.read_only,
                kind: p.config.kind,
                name: p.name,
                id: p.id,
            })
            .collect())
    }

    fn list_connections(&self) -> Result<ToolOutput> {
        let ws = self.engine.workspace();
        let results = self.engine.results_connection().ok().map(|p| p.id);
        let mut text = String::new();
        let mut shown = Vec::new();
        let connected = self.engine.connected_ids();
        for p in ws.list_connections()? {
            let is_results = results.as_deref() == Some(p.id.as_str());
            if !p.ai_policy.ai_enabled && !is_results {
                continue;
            }
            let env = serde_json::to_value(p.env).ok().and_then(|v| v.as_str().map(str::to_string)).filter(|e| e != "none");
            let indexed = ws.kn_count(&p.id).unwrap_or(0);
            text.push_str(&format!(
                "- \"{}\" · {}{}{}{}{}{}\n",
                p.name,
                p.config.kind.dialect_name(),
                env.as_deref().map(|e| format!(" · {e}")).unwrap_or_default(),
                if p.config.read_only { " · read-only" } else { "" },
                if connected.contains(&p.id) { " · connected" } else { "" },
                if p.id == self.profile.id { " · current" } else { "" },
                if is_results {
                    " · Results: DuckDB over outputs (results.<name>), use it to combine connections in a job".to_string()
                } else {
                    format!(" · {indexed} tables indexed")
                },
            ));
            shown.push(json!({"id": p.id, "name": p.name, "kind": p.config.kind}));
        }
        self.audit("list_connections", &json!({}), "allowed", Some(&format!("{} connections", shown.len())));
        Ok(out(text, json!({"connections": shown})))
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n { s.chars().take(n).collect::<String>() + "…" } else { s.to_string() }
}
