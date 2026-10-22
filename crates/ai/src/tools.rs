//! Tools the AI can call. Each tool is described by a JSON Schema (sent to
//! the model and exposed over MCP) and executed here with policy checks.

use std::sync::Arc;

use databrain_query_engine::{EventHub, QueryEngine, RunRequest};
use databrain_result_store::ViewSpec;
use databrain_workspace::{ConnectionProfile, KnNote, NoteStatus, Origin, SavedQuery};
use serde_json::{Value, json};

use crate::knowledge;
use crate::policy::{self, Decision, is_pii};
use crate::resultsql;
use crate::types::{AiError, Result, ToolSpec};

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

pub fn specs(include_editor: bool) -> Vec<ToolSpec> {
    let s = |name: &str, description: &str, parameters: Value| ToolSpec { name: name.into(), description: description.into(), parameters };
    let mut v = vec![
        s(
            "search_schema",
            "Search the connection's tables, views, columns and business notes by keywords. Returns the best matching tables as compact DDL. Use this before writing SQL when unsure which tables exist.",
            json!({"type": "object", "properties": {"query": {"type": "string", "description": "Keywords, e.g. 'monthly revenue by customer'"}, "connection": {"type": "string", "description": "Another connection (name from list_connections); default: the current one"}}, "required": ["query"]}),
        ),
        s(
            "describe_table",
            "Get columns, types, keys, comments and related tables for one table or view.",
            json!({"type": "object", "properties": {"table": {"type": "string", "description": "schema.table or table"}, "connection": {"type": "string", "description": "Another connection (name from list_connections); default: the current one"}}, "required": ["table"]}),
        ),
        s(
            "index_tables",
            "Add tables or views missing from the knowledge index: reads their columns, keys and comments from the database (after the user approves) and returns them. Use it when search_schema/describe_table can't find a table the user names, instead of asking the user for its structure.",
            json!({"type": "object", "properties": {
                "tables": {"type": "array", "items": {"type": "string"}, "description": "schema.table (catalog.schema.table on three-level engines) or a bare table name; up to 20"},
                "reason": {"type": "string", "description": "One line for the user: why these tables are needed"},
                "connection": {"type": "string", "description": "Another connection (name from list_connections); default: the current one"}
            }, "required": ["tables"]}),
        ),
        s(
            "list_tables",
            "List indexed table names (optionally only in one schema). Use search_schema for relevance ranking.",
            json!({"type": "object", "properties": {"schema": {"type": "string"}, "connection": {"type": "string", "description": "Another connection (name from list_connections); default: the current one"}}}),
        ),
        s(
            "get_sample_rows",
            "Fetch a few sample rows of a table to understand value formats (only if the connection allows sample data).",
            json!({"type": "object", "properties": {"table": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 20}}, "required": ["table"]}),
        ),
        s(
            "run_query",
            "Execute SQL on the current connection, or on another one with `connection` (not for results.<name> outputs: use query_outputs). Write it in that connection's dialect. Reads are row-capped; the user may need to approve. Returns a summary (columns, row count, column stats), a result_id for query_result and an output handle (results.<handle>) to combine results of several connections with query_outputs. Raw rows are only returned if the connection allows it.",
            json!({"type": "object", "properties": {"sql": {"type": "string"}, "connection": {"type": "string", "description": "Optional: name or id of another connection (see list_connections); default the current one"}, "purpose": {"type": "string", "description": "One short sentence shown to the user when asking for approval"}}, "required": ["sql"]}),
        ),
        s(
            "query_result",
            "Run SQLite SQL over one previous result stored locally as table `result` (e.g. SELECT region, sum(amount) FROM result GROUP BY 1). `result_id` may be a result id or an output handle/name (r12, revenue). Nothing is sent to the database.",
            json!({"type": "object", "properties": {"result_id": {"type": "string"}, "sql": {"type": "string"}}, "required": ["result_id", "sql"]}),
        ),
        s(
            "list_outputs",
            "List query outputs available in DataBrain (handle, name, source connection, SQL, columns, row count, whether capped). Outputs can come from different databases.",
            json!({"type": "object", "properties": {"filter": {"type": "string", "description": "Optional text to match in name/SQL/connection"}}}),
        ),
        s(
            "query_outputs",
            "Run DuckDB SQL locally over outputs referenced as results.<handle or name> (e.g. SELECT * FROM results.revenue r JOIN results.r12 c USING (country)). Joins outputs from different databases; nothing is sent to any database.",
            json!({"type": "object", "properties": {"sql": {"type": "string"}, "purpose": {"type": "string"}}, "required": ["sql"]}),
        ),
        s(
            "result_summary",
            "Columns, row count and per-column statistics of a stored result (current grid result if result_id is omitted).",
            json!({"type": "object", "properties": {"result_id": {"type": "string"}}}),
        ),
        s(
            "list_knowledge_notes",
            "List this connection's notes & glossary (id, target, text, status). Check it before adding a note, to update an existing one instead of duplicating it.",
            json!({"type": "object", "properties": {"filter": {"type": "string", "description": "Optional words to match in target or text"}}}),
        ),
        s(
            "add_knowledge_note",
            "Record a durable fact learned while exploring, for future questions: a business rule, the meaning of a code or status value, a join path, a data caveat (e.g. 'active customer = status in (1,2)'). One fact per note; no query results or personal data.",
            json!({"type": "object", "properties": {"target": {"type": "string", "description": "Existing table(s): schema.table or schema.table.column; several joined with & (e.g. join paths: sales.orders & crm.customers). Omit for glossary"}, "note": {"type": "string"}}, "required": ["note"]}),
        ),
        s(
            "update_knowledge_note",
            "Correct or extend an existing note (id from list_knowledge_notes) with the full new text, keeping facts that are still true. Also used to merge imported notes into existing ones.",
            json!({"type": "object", "properties": {"id": {"type": "string"}, "note": {"type": "string", "description": "Complete new text"}, "target": {"type": "string"}}, "required": ["id", "note"]}),
        ),
        s(
            "list_connections",
            "List the user's connections the assistant may use (name, engine, environment, read-only, connected), including the Results (DuckDB) connection that combines outputs. Pass a name as `connection` to run_query, list_tables, search_schema and describe_table to work on it; use it to answer across databases and to plan jobs.",
            json!({"type": "object", "properties": {}}),
        ),
        s(
            "list_jobs",
            "List the user's jobs (steps, schedule, last run status).",
            json!({"type": "object", "properties": {"filter": {"type": "string"}}}),
        ),
        s(
            "get_job",
            "Show one job in full: each step's action, connection, SQL, links (runs after), load/export settings, last run outcome and the errors of its latest run (with the failing SQL), plus its spec as JSON to edit with update_job.",
            json!({"type": "object", "properties": {"job": {"type": "string", "description": "Job name or id"}}, "required": ["job"]}),
        ),
        s(
            "create_job",
            "Create a job (asks the user to approve): SQL steps that run in order, by hand or on a schedule. Steps can run on different connections; a step on \"Results\" reads earlier steps' outputs as results.<step> with DuckDB SQL, to join data across databases. Then load steps write rows into a table of any connection and export steps save a file. Check table names with search_schema / describe_table (connection argument) first.",
            crate::jobs::create_job_schema(),
        ),
        s(
            "run_job",
            "Run an existing job now, or only some of its steps (asks the user to approve). Waits until it finishes (up to `wait_seconds`, default 300) and returns each step's status, rows, errors, and the outputs it produced (results.<step>, readable with query_outputs, e.g. to show the comparison step's rows). Use after create_job/update_job when the user wants the result, or to re-run a job.",
            json!({"type": "object", "properties": {
                "job": {"type": "string", "description": "Name or id of the job"},
                "steps": {"type": "array", "items": {"type": "string"}, "description": "Optional: only these steps (names), not the whole job"},
                "wait_seconds": {"type": "integer", "description": "How long to wait for it (10-1800, default 300)"}
            }, "required": ["job"]}),
        ),
        s(
            "update_job",
            "Change or fix an existing job (asks the user to approve, showing what changes): send the COMPLETE new list of steps in the create_job shape, starting from get_job's spec. Steps whose name is kept keep their place and history; steps left out are removed; use `rename` to rename steps (results.<old> in other steps follows). Omit schedule to keep it. Read the job and its latest errors with get_job first, and check tables with describe_table on the step's connection.",
            crate::jobs::update_job_schema(),
        ),
        s(
            "save_query",
            "Save a query to the user's saved queries (asks for approval).",
            json!({"type": "object", "properties": {"name": {"type": "string"}, "sql": {"type": "string"}, "description": {"type": "string"}}, "required": ["name", "sql"]}),
        ),
    ];
    if include_editor {
        v.push(s(
            "get_editor",
            "Read the SQL in the user's current editor tab, their selection, the last error, and the statement that failed (failed_sql).",
            json!({"type": "object", "properties": {}}),
        ));
        v.push(s(
            "write_editor",
            "Put SQL into the user's editor. mode: replace_selection | insert_at_cursor | replace_all | new_tab. The user sees a diff and accepts or rejects it.",
            json!({"type": "object", "properties": {"sql": {"type": "string"}, "mode": {"type": "string", "enum": ["replace_selection", "insert_at_cursor", "replace_all", "new_tab"]}, "title": {"type": "string"}}, "required": ["sql"]}),
        ));
    }
    v
}

/// Hooks into the host for tools that need the UI.
#[async_trait::async_trait]
pub trait ToolHost: Send + Sync {
    /// Ask the user to approve an action. Returns the (possibly edited)
    /// argument value on approval, `None` on denial.
    async fn approve(&self, tool: &str, summary: &str, detail: &Value) -> Option<Value>;
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
            // DuckDB with object storage: it reads and writes those URLs.
            let storage = p.config.opt("storage").filter(|s| !s.trim().is_empty()).map(|s| {
                let bucket = p.config.opt("storage_bucket").map(|b| format!(", bucket {b}")).unwrap_or_default();
                format!(" · object storage: {s}{bucket} (query by URL; export steps can write there with storage_connection)")
            });
            text.push_str(&format!(
                "- \"{}\" · {}{}{}{}{}{}{}\n",
                p.name,
                p.config.kind.dialect_name(),
                storage.unwrap_or_default(),
                env.as_deref().map(|e| format!(" · {e}")).unwrap_or_default(),
                if p.config.read_only { " · read-only" } else { "" },
                if connected.contains(&p.id) { " · connected" } else { "" },
                if p.id == self.profile.id { " · current" } else { "" },
                if is_results { " · Results: DuckDB over outputs (results.<name>), use it to combine connections in a job".to_string() } else { format!(" · {indexed} tables indexed") },
            ));
            shown.push(json!({"id": p.id, "name": p.name, "kind": p.config.kind}));
        }
        self.audit("list_connections", &json!({}), "allowed", Some(&format!("{} connections", shown.len())));
        Ok(out(text, json!({"connections": shown})))
    }

    fn list_jobs(&self, args: &Value) -> Result<ToolOutput> {
        let f = args.get("filter").and_then(|f| f.as_str()).unwrap_or("").to_lowercase();
        let jobs = self.engine.workspace().list_jobs()?;
        let mut text = String::new();
        for j in jobs.iter().filter(|j| f.is_empty() || j.name.to_lowercase().contains(&f) || j.step_names.iter().any(|n| n.to_lowercase().contains(&f))) {
            let last = j.last_run.as_ref().map(|r| format!(" · last run {}", r.status)).unwrap_or_default();
            let sch = if j.schedule.enabled { " · scheduled" } else { "" };
            text.push_str(&format!("- \"{}\" (id {}): steps {}{sch}{last}\n", j.name, j.id, j.step_names.join(", ")));
        }
        if text.is_empty() {
            text = "No jobs.".into();
        }
        self.audit("list_jobs", args, "allowed", None);
        Ok(out(text, json!({"count": jobs.len()})))
    }

    /// The job a tool argument names (name or id).
    fn find_job(&self, want: &str) -> Result<databrain_workspace::Job> {
        let ws = self.engine.workspace();
        let id = ws
            .list_jobs()?
            .into_iter()
            .find(|j| j.id == want || j.name.eq_ignore_ascii_case(want))
            .map(|j| j.id)
            .ok_or_else(|| AiError::Policy(format!("no job \"{want}\" (see list_jobs)")))?;
        Ok(ws.get_job(&id)?)
    }

    fn get_job(&self, args: &Value) -> Result<ToolOutput> {
        let job = self.find_job(arg(args, "job")?)?;
        let ws = self.engine.workspace();
        let refs = self.conn_refs()?;
        let mut text = crate::jobs::describe(&job, &refs);
        // Latest run: what failed, with the statement (to fix it).
        if let Some(run) = ws.job_runs(&job.id, 1)?.into_iter().next() {
            text.push_str(&format!("\nLatest run ({}, {}){}\n", run.trigger, run.status, run.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()));
            let log = ws.job_run_log(&job.id, run.id).unwrap_or_default();
            let bad: Vec<_> = log.iter().filter(|l| l.level == "error" || l.level == "warning").collect();
            for l in bad.iter().rev().take(15).rev() {
                text.push_str(&format!("- [{}] {}{}\n", l.level, l.step.as_deref().map(|s| format!("{s}: ")).unwrap_or_default(), trunc(&l.message, 600)));
                if let Some(sql) = &l.sql {
                    text.push_str(&format!("  statement: {}\n", trunc(sql, 1500)));
                }
            }
        }
        text.push_str(&format!("\nSpec (edit and send to update_job):\n{}\n", serde_json::to_string_pretty(&crate::jobs::spec_of(&job, &refs)).unwrap_or_default()));
        self.audit("get_job", args, "allowed", None);
        Ok(out(text, json!({"job_id": job.id, "name": job.name})))
    }

    async fn run_job(&self, args: &Value) -> Result<ToolOutput> {
        let job = self.find_job(arg(args, "job")?)?;
        let refs = self.conn_refs()?;
        let want: Vec<String> = args.get("steps").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_string())).collect()).unwrap_or_default();
        let mut only_ids = Vec::new();
        for w in &want {
            let n = job.nodes.iter().find(|n| n.name.eq_ignore_ascii_case(w) || n.id == *w).ok_or_else(|| AiError::Policy(format!("job \"{}\" has no step \"{w}\"", job.name)))?;
            only_ids.push(n.id.clone());
        }
        let scope = if want.is_empty() { "all steps".to_string() } else { format!("steps {}", want.join(", ")) };
        // What a run changes: loads write tables, exports write files.
        let writes: Vec<String> = job
            .nodes
            .iter()
            .filter(|n| only_ids.is_empty() || only_ids.contains(&n.id))
            .filter_map(|n| match n.kind {
                databrain_workspace::JobNodeKind::Load => Some(format!("{} loads into {}", n.name, n.target_table.as_deref().unwrap_or("a table"))),
                databrain_workspace::JobNodeKind::Export => Some(format!("{} writes a {} file", n.name, n.export_format.extension())),
                _ => None,
            })
            .collect();
        let preview = format!(
            "Runs {scope} of \"{}\".{}\n\n{}",
            job.name,
            if writes.is_empty() { " It only reads data.".to_string() } else { format!(" It changes data: {}.", writes.join("; ")) },
            crate::jobs::describe(&job, &refs)
        );
        self.gate("run_job", Decision::Ask("Run a job".into()), &format!("\"{}\" ({scope})", job.name), &json!({"preview": preview, "name": job.name}))
            .await?;
        let run_id = self.host.start_job(&job.id, (!only_ids.is_empty()).then_some(only_ids)).await.map_err(AiError::Policy)?;
        let wait = args.get("wait_seconds").and_then(|v| v.as_u64()).unwrap_or(300).clamp(10, 1800);
        let ws = self.engine.workspace();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);
        let run = loop {
            let r = ws.job_runs(&job.id, 20)?.into_iter().find(|r| r.id == run_id);
            if let Some(r) = r.filter(|r| r.status != "running") {
                break Some(r);
            }
            if tokio::time::Instant::now() > deadline {
                break None;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return Err(AiError::Cancelled),
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            }
        };
        let Some(run) = run else {
            return Ok(out(
                format!("Job \"{}\" is still running after {wait} s; it keeps running. Check it later with get_job.", job.name),
                json!({"job_id": job.id, "name": job.name, "run_id": run_id, "running": true}),
            ));
        };
        let mut text = format!("Job \"{}\" finished: {}{}\n", job.name, run.status, run.error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default());
        for n in &run.nodes {
            let st = &n.summary;
            text.push_str(&format!(
                "- {}: {}{}{}{}\n",
                n.name,
                st.status,
                st.rows.map(|r| format!(", {r} rows")).unwrap_or_default(),
                st.error.as_deref().map(|e| format!(", error: {}", trunc(e, 600))).unwrap_or_default(),
                if st.status == "success" && self.engine.outputs().resolve(&n.name).is_some() { format!(" → output results.{}", n.name) } else { String::new() },
            ));
        }
        if run.status != "success" {
            text.push_str("Use get_job to see the failing statement.\n");
        }
        Ok(out(text, json!({"job_id": job.id, "name": job.name, "run_id": run_id, "status": run.status})))
    }

    async fn update_job(&self, args: &Value) -> Result<ToolOutput> {
        let old = self.find_job(arg(args, "job")?)?;
        let ws = self.engine.workspace();
        let taken: std::collections::HashSet<String> = ws.job_step_names()?.into_iter().filter(|(_, o)| o.job_id != old.id).map(|(n, _)| n).collect();
        let refs = self.conn_refs()?;
        let (job, notes) = crate::jobs::merge_job(&old, args, &refs, &taken)?;
        let changes = crate::jobs::changes(&old, &job);
        if changes.is_empty() {
            return Ok(out("Nothing changes: the job is already like that.", json!({"job_id": old.id, "name": old.name})));
        }
        let preview = format!("Changes:\n{}\n\nAfter the change:\n{}", changes.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n"), crate::jobs::describe(&job, &refs));
        self.gate("update_job", Decision::Ask("Change a job".into()), &format!("\"{}\": {}", old.name, changes.join("; ")), &json!({"preview": preview, "name": old.name}))
            .await?;
        let saved = ws.save_job(job)?;
        let mut text = format!("Updated job \"{}\". It is not run; the user runs it (Run) or its schedule does.\n{preview}", saved.name);
        for n in notes {
            text.push_str(&format!("\nNote: {n}"));
        }
        Ok(out(text, json!({"job_id": saved.id, "name": saved.name, "updated": true})))
    }

    async fn create_job(&self, args: &Value) -> Result<ToolOutput> {
        let ws = self.engine.workspace();
        let taken: std::collections::HashSet<String> = ws.job_step_names()?.into_iter().map(|(n, _)| n).collect();
        let refs = self.conn_refs()?;
        let (job, notes) = crate::jobs::build_job(args, &refs, &taken)?;
        let preview = crate::jobs::describe(&job, &refs);
        self.gate("create_job", Decision::Ask("Create a job".into()), &format!("\"{}\" with {} steps", job.name, job.nodes.len()), &json!({"preview": preview, "name": job.name}))
            .await?;
        let saved = ws.save_job(job)?;
        let mut text = format!("Created job \"{}\" (id {}). It is not run yet; the user runs it from its tab (Run) or the schedule does.\n{preview}", saved.name, saved.id);
        for n in notes {
            text.push_str(&format!("Note: {n}\n"));
        }
        Ok(out(text, json!({"job_id": saved.id, "name": saved.name})))
    }

    fn policy(&self) -> &databrain_workspace::AiPolicy {
        &self.profile.ai_policy
    }

    fn search_schema(&self, args: &Value) -> Result<ToolOutput> {
        let q = arg(args, "query")?;
        let target = self.schema_target(args)?;
        let r = knowledge::retrieve(self.engine.workspace(), &target.id, q, 8, 12_000)?;
        self.audit("search_schema", args, "allowed", Some(&format!("{} tables", r.tables.len())));
        if r.tables.is_empty() {
            let hint = if self.engine.workspace().kn_count(&target.id)? == 0 {
                "The knowledge index is empty. Use index_tables for the tables the question names (don't ask the user for their columns); for broad questions, suggest indexing the connection (AI panel → Knowledge → Index)."
            } else {
                "No matching tables in the index. Try other keywords or list_tables; if the user named a table, index_tables reads it from the database."
            };
            return Ok(out(hint, json!({"tables": []})));
        }
        let mut text = r.text.clone();
        if !r.notes.is_empty() {
            text.push_str("\n\nGlossary:\n");
            for n in &r.notes {
                text.push_str(&format!("- {n}\n"));
            }
        }
        Ok(out(text, json!({"tables": r.tables})))
    }

    fn describe_table(&self, args: &Value) -> Result<ToolOutput> {
        let t = arg(args, "table")?;
        let target = self.schema_target(args)?;
        let ws = self.engine.workspace();
        let o = ws.kn_get(&target.id, t)?.ok_or_else(|| AiError::Policy(format!("table `{t}` is not in the knowledge index; search_schema with other words, or index_tables to read it from the database (don't ask the user for its columns)")))?;
        let full = o.full_name();
        let notes: Vec<String> = ws
            .kn_notes(&target.id)?
            .into_iter()
            .filter(|n| n.status == NoteStatus::Approved && n.target.as_deref().is_some_and(|x| databrain_workspace::target_mentions(x, &full, &o.name)))
            .map(|n| match n.target.as_deref() {
                Some(t) if !t.eq_ignore_ascii_case(&full) => format!("{t}: {}", n.body),
                _ => n.body,
            })
            .collect();
        let pol = target.ai_policy.clone();
        let mut text = knowledge::render_object(&o, &|c| is_pii(&pol, Some(&full), c), &notes);
        // Tables that reference this one.
        let refs: Vec<String> = ws
            .kn_objects(&target.id)?
            .into_iter()
            .filter(|x| x.foreign_keys.iter().any(|f| f.ref_table.eq_ignore_ascii_case(&o.name)))
            .map(|x| x.full_name())
            .take(10)
            .collect();
        if !refs.is_empty() {
            text.push_str(&format!("\nReferenced by: {}", refs.join(", ")));
        }
        self.audit("describe_table", args, "allowed", None);
        Ok(out(text, json!({"table": full})))
    }

    async fn index_tables(&self, args: &Value) -> Result<ToolOutput> {
        let target = self.schema_target(args)?;
        let tables: Vec<String> = match args.get("tables") {
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect(),
            Some(Value::String(t)) => t.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect(),
            _ => vec![],
        };
        if tables.is_empty() {
            return Err(AiError::Policy("missing argument `tables`".into()));
        }
        if tables.len() > 20 {
            return Err(AiError::Policy("index at most 20 tables at a time; for whole schemas, ask the user to index the connection (AI panel → Knowledge)".into()));
        }
        let reason = args.get("reason").and_then(|r| r.as_str()).unwrap_or("").trim();
        let summary = format!(
            "Read the structure (columns, keys, comments) of {} from \"{}\" into the knowledge index{}. No data is read.",
            tables.join(", "),
            target.name,
            if reason.is_empty() { String::new() } else { format!(" ({reason})") }
        );
        let detail = json!({"tables": tables, "connection": target.name});
        self.gate("index_tables", Decision::Ask("Index tables".into()), &summary, &detail).await?;
        let results = knowledge::index_tables(&self.engine, &target.id, &tables, &self.cancel).await?;
        let pol = target.ai_policy.clone();
        let mut text = String::new();
        let mut ok = Vec::new();
        let mut failed = Vec::new();
        for (want, r) in &results {
            match r {
                Ok(o) => {
                    let full = o.full_name();
                    text.push_str(&knowledge::render_object(o, &|c| is_pii(&pol, Some(&full), c), &[]));
                    text.push('\n');
                    ok.push(full);
                }
                Err(e) => {
                    text.push_str(&format!("-- {want}: not indexed: {e}\n"));
                    failed.push(json!({"table": want, "error": e}));
                }
            }
        }
        self.audit("index_tables", args, "approved", Some(&format!("{} indexed, {} failed", ok.len(), failed.len())));
        Ok(out(format!("Indexed {} of {} tables.\n{text}", ok.len(), results.len()), json!({"indexed": ok, "failed": failed, "connection": target.name})))
    }

    fn list_tables(&self, args: &Value) -> Result<ToolOutput> {
        let schema = args.get("schema").and_then(|s| s.as_str());
        let target = self.schema_target(args)?;
        let names: Vec<String> = self
            .engine
            .workspace()
            .kn_objects(&target.id)?
            .into_iter()
            .filter(|o| schema.is_none_or(|s| o.schema.eq_ignore_ascii_case(s)))
            .map(|o| o.full_name())
            .take(500)
            .collect();
        self.audit("list_tables", args, "allowed", Some(&format!("{} tables", names.len())));
        Ok(out(if names.is_empty() { "No indexed tables.".into() } else { names.join("\n") }, json!({"count": names.len()})))
    }

    async fn gate(&self, tool: &str, decision: Decision, summary: &str, detail: &Value) -> Result<Value> {
        match decision {
            Decision::Allow => {
                self.audit(tool, detail, "allowed", None);
                Ok(detail.clone())
            }
            Decision::Deny(reason) => Err(AiError::Policy(reason)),
            Decision::Ask(reason) => {
                if self.caller == Caller::Mcp {
                    // The MCP client (Kiro CLI, Claude Code…) asked its user
                    // before calling the tool: that is the approval. DataBrain's
                    // hard rules (Deny: read-only, production, writes not
                    // allowed, queries off) were applied above.
                    self.audit(tool, detail, "approved by MCP client", Some(&reason));
                    return Ok(detail.clone());
                }
                match self.host.approve(tool, &format!("{reason}. {summary}"), detail).await {
                    Some(v) => {
                        self.audit(tool, &v, "approved", None);
                        Ok(v)
                    }
                    None => {
                        self.audit(tool, detail, "denied", None);
                        Err(AiError::Policy("the user declined".into()))
                    }
                }
            }
        }
    }

    async fn run_query(&self, args: &Value) -> Result<ToolOutput> {
        let sql = arg(args, "sql")?.to_string();
        // `connection`: another connection (its own policy applies).
        let target = self.schema_target(args)?;
        // results.<output> only exists on the Results (DuckDB) connection:
        // such SQL runs there, never on the current connection.
        if target.config.kind != databrain_connector_core::ConnectorKind::Duckdb && !self.engine.outputs().referenced(&sql).is_empty() {
            return self.query_outputs(args).await;
        }
        let purpose = args.get("purpose").and_then(|p| p.as_str()).unwrap_or("");
        let review = policy::review_sql(&target, &sql);
        let on = if target.id == self.profile.id { String::new() } else { format!(" (on \"{}\")", target.name) };
        let approved = self
            .gate("run_query", review.decision.clone(), &format!("{purpose}{on}"), &json!({"sql": sql, "purpose": purpose, "kind": review.kind, "connection": target.name}))
            .await?;
        let sql = approved.get("sql").and_then(|s| s.as_str()).unwrap_or(&sql).to_string();
        // Re-review edited SQL.
        if let Decision::Deny(r) = policy::review_sql(&target, &sql).decision {
            return Err(AiError::Policy(r));
        }
        let cap = 1000usize;
        let run_sql = if review.kind.is_read() { policy::cap_rows(target.config.kind, &sql, cap) } else { sql.clone() };
        let tab = format!("ai:{}", self.session_id.clone().unwrap_or_else(|| "mcp".into()));
        let outcomes = self
            .engine
            .run_and_wait(
                &self.hub,
                RunRequest {
                    connection_id: target.id.clone(),
                    tab_id: tab,
                    sql: run_sql,
                    base_offset: 0,
                    row_limit: Some(cap),
                    confirmed: true,
                    origin: if self.caller == Caller::Mcp { Origin::Mcp } else { Origin::Ai },
                    session_key: None,
                    output_name: None,
                    params: Default::default(),
                },
                Some(self.cancel.clone()),
            )
            .await?;
        let mut text = String::new();
        let mut display = Vec::new();
        for o in &outcomes {
            if let Some(e) = &o.error {
                text.push_str(&format!("Statement {} failed: {}\n", o.index + 1, e.message));
                display.push(json!({"index": o.index, "error": e.message}));
                continue;
            }
            match &o.result {
                Some(r) => {
                    self.results.lock().push(r.id.clone());
                    let handle = self.engine.outputs().by_result(&r.id).map(|o| format!(" Output {} (query it as {}).", o.handle, o.reference())).unwrap_or_default();
                    text.push_str(&format!("Statement {} returned {} rows{} in {} ms. result_id={}{handle}\n", o.index + 1, r.total_rows, if r.truncated { " (capped)" } else { "" }, o.duration_ms, r.id));
                    text.push_str(&self.summarize(&r.id)?);
                    display.push(json!({"index": o.index, "result": r, "duration_ms": o.duration_ms}));
                }
                None => {
                    text.push_str(&format!("Statement {} OK{}.\n", o.index + 1, o.rows_affected.map(|n| format!(", {n} rows affected")).unwrap_or_default()));
                    display.push(json!({"index": o.index, "rows_affected": o.rows_affected}));
                }
            }
        }
        self.audit("run_query", &json!({"sql": sql, "connection": target.name}), "executed", Some(text.lines().next().unwrap_or("")));
        Ok(out(format!("{}{text}", if on.is_empty() { String::new() } else { format!("Ran{on}.\n") }), json!({"sql": sql, "statements": display, "connection_id": target.id})))
    }

    /// Schema + stats + (optionally) first rows, masked per policy.
    /// Policy that governs a result: its source connection's (data is
    /// shared on that connection's terms), falling back to this one's.
    fn source_policy(&self, result_id: &str) -> Result<databrain_workspace::AiPolicy> {
        let Some(o) = self.engine.outputs().by_result(result_id) else { return Ok(self.policy().clone()) };
        if o.connection_id == self.profile.id {
            return Ok(self.policy().clone());
        }
        let src = self.engine.workspace().get_connection(&o.connection_id).map_err(|_| AiError::Policy(format!("source connection of {} no longer exists", o.handle)))?;
        if !src.ai_policy.ai_enabled {
            return Err(AiError::Policy(format!("{} comes from \"{}\", where AI is disabled", o.handle, src.name)));
        }
        if self.caller == Caller::Mcp && !src.ai_policy.mcp_enabled {
            return Err(AiError::Policy(format!("{} comes from \"{}\", which is not shared with MCP", o.handle, src.name)));
        }
        Ok(src.ai_policy)
    }

    /// Accept a result id or an output reference (`r12`, `revenue`, `results.x`).
    fn resolve_result(&self, r: &str) -> Result<String> {
        if self.engine.results().contains(r) {
            return Ok(r.to_string());
        }
        let o = self.engine.outputs().ensure_loaded(r).map_err(|e| AiError::Policy(e.message))?;
        Ok(o.result_id)
    }

    fn summarize(&self, result_id: &str) -> Result<String> {
        let pol = self.source_policy(result_id)?;
        let rs = self.engine.results().get(result_id).map_err(|e| AiError::Internal(e.to_string()))?;
        let mut g = rs.lock();
        let info = g.info();
        let view = ViewSpec::default();
        let mut s = String::from("Columns:\n");
        for (i, c) in info.columns.iter().enumerate() {
            let pii = is_pii(&pol, None, &c.name);
            s.push_str(&format!("- {} ({})", c.name, c.db_type.clone().unwrap_or_else(|| c.data_type.clone())));
            if !pii && info.total_rows > 0 {
                if let Ok(st) = g.column_stats(&view, i) {
                    s.push_str(&format!(" nulls={} distinct={}", st.nulls, st.distinct));
                    if pol.share_sample_values || pol.share_result_rows {
                        if let (Some(mn), Some(mx)) = (&st.min, &st.max) {
                            s.push_str(&format!(" min={} max={}", trunc(mn, 40), trunc(mx, 40)));
                        }
                    }
                }
            } else if pii {
                s.push_str(" [PII]");
            }
            s.push('\n');
        }
        if pol.share_result_rows && info.total_rows > 0 {
            let n = (pol.max_rows_to_model as usize).min(info.total_rows);
            let page = g.page(&view, 0, n).map_err(|e| AiError::Internal(e.to_string()))?;
            s.push_str(&format!("First {n} rows (TSV):\n"));
            s.push_str(&info.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("\t"));
            s.push('\n');
            for row in page.rows {
                let cells: Vec<String> = row
                    .iter()
                    .zip(&info.columns)
                    .map(|(v, c)| if is_pii(&pol, None, &c.name) { "***".into() } else { v.as_deref().map(|x| trunc(x, 80)).unwrap_or_else(|| "NULL".into()) })
                    .collect();
                s.push_str(&cells.join("\t"));
                s.push('\n');
            }
        } else if info.total_rows > 0 {
            s.push_str("(Raw rows are not shared on this connection; use query_result to aggregate locally.)\n");
        }
        Ok(s)
    }

    fn result_summary(&self, args: &Value) -> Result<ToolOutput> {
        let id = args
            .get("result_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| self.host.current_result())
            .ok_or_else(|| AiError::Policy("no result available".into()))?;
        let id = self.resolve_result(&id)?;
        let text = self.summarize(&id)?;
        self.audit("result_summary", args, "allowed", None);
        Ok(out(format!("result_id={id}\n{text}"), json!({"result_id": id})))
    }

    fn allowed_result(&self, id: &str) -> bool {
        self.results.lock().iter().any(|r| r == id) || self.host.current_result().as_deref() == Some(id)
    }

    fn query_result(&self, args: &Value) -> Result<ToolOutput> {
        let id = self.resolve_result(arg(args, "result_id")?)?;
        let id = id.as_str();
        let sql = arg(args, "sql")?;
        // Results of this conversation, the user's current/mentioned ones,
        // or any output the source connection lets the AI read.
        if !self.allowed_result(id) && !self.source_policy(id)?.share_result_rows {
            return Err(AiError::Policy("this output was not shared with the assistant (mention it with @, or enable result rows on its connection)".into()));
        }
        let rs = self.engine.results().get(id).map_err(|e| AiError::Internal(e.to_string()))?;
        let (names, batches) = {
            let mut g = rs.lock();
            let names: Vec<String> = g.schema().fields().iter().map(|f| f.name().clone()).collect();
            (names, g.view_batches(&ViewSpec::default(), 50_000).map_err(|e| AiError::Internal(e.to_string()))?)
        };
        let pol = self.source_policy(id)?;
        // Block PII columns from being selected in local aggregates' output.
        if names.iter().any(|n| is_pii(&pol, None, n) && sql.to_ascii_lowercase().contains(&n.to_ascii_lowercase())) {
            return Err(AiError::Policy("the query references a PII column".into()));
        }
        let t = resultsql::query(&names, &batches, sql, 200)?;
        let mut text = t.columns.join("\t");
        text.push('\n');
        for r in &t.rows {
            text.push_str(&r.iter().map(|v| match v { Value::String(s) => trunc(s, 80), Value::Null => "NULL".into(), o => o.to_string() }).collect::<Vec<_>>().join("\t"));
            text.push('\n');
        }
        if t.truncated {
            text.push_str("(first 200 rows)\n");
        }
        self.audit("query_result", args, "allowed", Some(&format!("{} rows", t.rows.len())));
        Ok(out(text, json!({"columns": t.columns, "rows": t.rows, "truncated": t.truncated})))
    }

    fn list_outputs(&self, args: &Value) -> Result<ToolOutput> {
        let filter = args.get("filter").and_then(|f| f.as_str()).unwrap_or("").to_lowercase();
        let mut lines = Vec::new();
        let mut shown = Vec::new();
        for o in self.engine.outputs().list() {
            if o.state == databrain_query_engine::OutputState::Evicted || self.source_policy(&o.result_id).is_err() {
                continue;
            }
            let hay = format!("{} {} {} {}", o.handle, o.name.clone().unwrap_or_default(), o.connection_name, o.sql).to_lowercase();
            if !filter.is_empty() && !hay.contains(&filter) {
                continue;
            }
            let cols: Vec<String> = o.columns.iter().take(30).map(|c| format!("{} {}", c.name, c.db_type.clone().unwrap_or_else(|| c.data_type.clone()))).collect();
            lines.push(format!(
                "- {} ({}){} — {} rows{} from \"{}\"\n  columns: {}\n  sql: {}",
                o.reference(),
                o.handle,
                if self.allowed_result(&o.result_id) { " [in this conversation]" } else { "" },
                o.rows,
                if o.truncated { " (capped at the row limit — incomplete)" } else { "" },
                o.connection_name,
                cols.join(", "),
                trunc(&o.sql.replace('\n', " "), 300)
            ));
            shown.push(o.handle.clone());
            if lines.len() >= 40 {
                break;
            }
        }
        self.audit("list_outputs", args, "allowed", Some(&format!("{} outputs", lines.len())));
        let text = if lines.is_empty() { "No outputs available. Run a query first.".to_string() } else { lines.join("\n") };
        Ok(out(text, json!({"outputs": shown})))
    }

    async fn query_outputs(&self, args: &Value) -> Result<ToolOutput> {
        let sql = arg(args, "sql")?.to_string();
        let refs = self.engine.outputs().referenced(&sql);
        if refs.is_empty() {
            return Err(AiError::Policy("reference outputs as results.<handle or name> (see list_outputs)".into()));
        }
        for o in &refs {
            let pol = self.source_policy(&o.result_id)?;
            if !pol.share_result_rows && !self.allowed_result(&o.result_id) {
                return Err(AiError::Policy(format!("{} is from \"{}\", which does not share result rows with the assistant; mention it with @ to allow it", o.handle, o.connection_name)));
            }
            if o.columns.iter().any(|c| is_pii(&pol, None, &c.name) && sql.to_ascii_lowercase().contains(&c.name.to_ascii_lowercase())) {
                return Err(AiError::Policy(format!("the query references a sensitive column of {}", o.handle)));
            }
        }
        // Local DuckDB, but still reads only.
        let results_conn = self.engine.results_connection().map_err(|e| AiError::Internal(e.message))?;
        if !policy::review_sql(&results_conn, &sql).kind.is_read() {
            return Err(AiError::Policy("query_outputs only runs read-only SQL".into()));
        }
        let outcomes = self
            .engine
            .run_and_wait(
                &self.hub,
                RunRequest {
                    connection_id: results_conn.id.clone(),
                    tab_id: format!("ai-outputs:{}", self.session_id.clone().unwrap_or_else(|| "mcp".into())),
                    sql: policy::cap_rows(databrain_connector_core::ConnectorKind::Duckdb, &sql, 1000),
                    base_offset: 0,
                    row_limit: Some(1000),
                    confirmed: true,
                    origin: if self.caller == Caller::Mcp { Origin::Mcp } else { Origin::Ai },
                    session_key: None,
                    output_name: None,
                    params: Default::default(),
                },
                Some(self.cancel.clone()),
            )
            .await?;
        let mut text = String::new();
        let mut display = Vec::new();
        for o in &outcomes {
            if let Some(e) = &o.error {
                text.push_str(&format!("ERROR: {}\n", e.message));
                continue;
            }
            for n in &o.notices {
                text.push_str(&format!("Note: {n}\n"));
            }
            if let Some(r) = &o.result {
                self.results.lock().push(r.id.clone());
                let handle = self.engine.outputs().by_result(&r.id).map(|x| x.handle).unwrap_or_default();
                text.push_str(&format!("{} rows (output {handle}).\n", r.total_rows));
                // Rows go back to the model: allowed because every source shares them (checked above).
                let rs = self.engine.results().get(&r.id).map_err(|e| AiError::Internal(e.to_string()))?;
                let page = rs.lock().page(&ViewSpec::default(), 0, 200).map_err(|e| AiError::Internal(e.to_string()))?;
                text.push_str(&r.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("\t"));
                text.push('\n');
                for row in page.rows {
                    text.push_str(&row.iter().map(|v| v.as_deref().map(|s| trunc(s, 80)).unwrap_or_else(|| "NULL".into())).collect::<Vec<_>>().join("\t"));
                    text.push('\n');
                }
                if r.total_rows > 200 {
                    text.push_str("(first 200 rows)\n");
                }
                display.push(json!({"index": o.index, "result": r, "duration_ms": o.duration_ms}));
            }
        }
        self.audit("query_outputs", &json!({"sql": sql}), "executed", Some(&format!("{} outputs", refs.len())));
        Ok(out(text, json!({"sql": sql, "statements": display})))
    }

    async fn sample_rows(&self, args: &Value) -> Result<ToolOutput> {
        if !self.policy().share_sample_values && !self.policy().share_result_rows {
            return Err(AiError::Policy("sharing sample data is disabled for this connection".into()));
        }
        let t = arg(args, "table")?;
        let o = self
            .engine
            .workspace()
            .kn_get(&self.profile.id, t)?
            .ok_or_else(|| AiError::Policy(format!("table `{t}` not found")))?;
        let n = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(5).clamp(1, 20) as usize;
        let kind = self.profile.config.kind;
        let full = o.full_name();
        let sql = databrain_connector_core::traits::preview_query(kind, &o.schema, &o.name, n);
        // Sampling is a read the user enabled via policy; no approval prompt.
        let outcomes = self
            .engine
            .run_and_wait(
                &self.hub,
                RunRequest {
                    connection_id: self.profile.id.clone(),
                    tab_id: format!("ai-sample:{}", self.session_id.clone().unwrap_or_default()),
                    sql,
                    base_offset: 0,
                    row_limit: Some(n),
                    confirmed: true,
                    origin: Origin::Ai,
                    session_key: None,
                    output_name: None,
                    params: Default::default(),
                },
                Some(self.cancel.clone()),
            )
            .await?;
        let o = outcomes.into_iter().next().ok_or_else(|| AiError::Internal("no result".into()))?;
        if let Some(e) = o.error {
            return Err(AiError::Policy(e.message));
        }
        let r = o.result.ok_or_else(|| AiError::Internal("no rows".into()))?;
        let rs = self.engine.results().get(&r.id).map_err(|e| AiError::Internal(e.to_string()))?;
        let page = rs.lock().page(&ViewSpec::default(), 0, n).map_err(|e| AiError::Internal(e.to_string()))?;
        let pol = self.policy().clone();
        let mut text = r.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("\t");
        text.push('\n');
        for row in page.rows {
            text.push_str(
                &row.iter()
                    .zip(&r.columns)
                    .map(|(v, c)| if is_pii(&pol, Some(&full), &c.name) { "***".into() } else { v.as_deref().map(|x| trunc(x, 60)).unwrap_or_else(|| "NULL".into()) })
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
            text.push('\n');
        }
        self.engine.results().remove(&r.id);
        self.audit("get_sample_rows", args, "allowed", None);
        Ok(out(text, json!({"table": t})))
    }

    /// Saved directly when the connection auto-approves AI notes, else proposed.
    fn ai_note_status(&self) -> NoteStatus {
        if self.policy().auto_approve_notes { NoteStatus::Approved } else { NoteStatus::Proposed }
    }

    fn note_saved_text(&self, status: NoteStatus, what: &str) -> String {
        match status {
            NoteStatus::Approved => format!("Note {what} and saved."),
            NoteStatus::Proposed => format!("Note {what}; the user will review it in Knowledge before it is used."),
        }
    }

    /// Validated, canonical target (tables must exist in this connection).
    async fn checked_target(&self, target: Option<&str>) -> Result<Option<String>> {
        match target.map(str::trim).filter(|t| !t.is_empty()) {
            None => Ok(None),
            Some(t) => knowledge::check_note_target(&self.engine, &self.profile.id, t)
                .await
                .map(Some)
                .map_err(|e| AiError::Policy(format!("invalid target `{t}`: {e}. Use real tables (schema.table, several joined with &), or omit target for a glossary note"))),
        }
    }

    async fn add_note(&self, args: &Value) -> Result<ToolOutput> {
        let note = arg(args, "note")?;
        let target = self.checked_target(args.get("target").and_then(|t| t.as_str())).await?;
        let ws = self.engine.workspace();
        // Same text already there: nothing to add.
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        if let Some(dup) = ws.kn_notes(&self.profile.id)?.into_iter().find(|n| norm(&n.body) == norm(note)) {
            return Ok(out(format!("An identical note already exists (id {}).", dup.id), json!({"note": dup})));
        }
        let status = self.ai_note_status();
        let saved = ws.kn_save_note(KnNote {
            id: String::new(),
            connection_id: self.profile.id.clone(),
            target,
            body: note.to_string(),
            author: "ai".into(),
            status,
            created_at: 0,
            replaces: None,
        })?;
        self.audit("add_knowledge_note", args, status.as_str(), None);
        Ok(out(self.note_saved_text(status, "added"), json!({"note": saved})))
    }

    async fn update_note(&self, args: &Value) -> Result<ToolOutput> {
        let id = arg(args, "id")?;
        let note = arg(args, "note")?;
        let ws = self.engine.workspace();
        let old = ws
            .kn_notes(&self.profile.id)?
            .into_iter()
            .find(|n| n.id == id)
            .ok_or_else(|| AiError::Policy(format!("note {id} not found; use list_knowledge_notes")))?;
        let target = match args.get("target").and_then(|t| t.as_str()) {
            Some(t) => self.checked_target(Some(t)).await?,
            None => old.target.clone(),
        };
        let status = self.ai_note_status();
        let saved = if old.status == NoteStatus::Proposed {
            // Revising its own pending proposal: edit it in place.
            ws.kn_save_note(KnNote { body: note.to_string(), target, ..old.clone() })?
        } else {
            ws.kn_save_note(KnNote {
                id: String::new(),
                connection_id: self.profile.id.clone(),
                target,
                body: note.to_string(),
                author: "ai".into(),
                status,
                created_at: 0,
                replaces: Some(old.id.clone()),
            })?
        };
        self.audit("update_knowledge_note", args, status.as_str(), None);
        Ok(out(self.note_saved_text(saved.status, "updated"), json!({"note": saved, "previous": old})))
    }

    fn list_notes(&self, args: &Value) -> Result<ToolOutput> {
        let filter = args.get("filter").and_then(|f| f.as_str()).unwrap_or("").to_lowercase();
        let words: Vec<&str> = filter.split_whitespace().collect();
        let notes: Vec<KnNote> = self
            .engine
            .workspace()
            .kn_notes(&self.profile.id)?
            .into_iter()
            .filter(|n| {
                let hay = format!("{} {}", n.target.as_deref().unwrap_or(""), n.body).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .take(200)
            .collect();
        self.audit("list_knowledge_notes", args, "allowed", Some(&format!("{} notes", notes.len())));
        if notes.is_empty() {
            return Ok(out("No notes match.", json!({"notes": []})));
        }
        let mut text = String::new();
        for n in &notes {
            let st = if n.status == NoteStatus::Proposed { " (pending review)" } else { "" };
            text.push_str(&format!("- id {} · {}{st}: {}\n", n.id, n.target.as_deref().unwrap_or("glossary"), trunc(&n.body, 600)));
        }
        Ok(out(text, json!({"notes": notes})))
    }

    async fn save_query(&self, args: &Value) -> Result<ToolOutput> {
        let v = self.gate("save_query", Decision::Ask("Save query".into()), arg(args, "name")?, args).await?;
        let saved = self.engine.workspace().save_query(SavedQuery {
            id: String::new(),
            name: v.get("name").and_then(|n| n.as_str()).unwrap_or("AI query").to_string(),
            sql: v.get("sql").and_then(|n| n.as_str()).unwrap_or_default().to_string(),
            connection_id: Some(self.profile.id.clone()),
            folder_id: None,
            description: v.get("description").and_then(|d| d.as_str()).map(str::to_string),
            tags: vec!["ai".into()],
            ai_example: false,
            created_at: 0,
            updated_at: 0,
        })?;
        Ok(out(format!("Saved as \"{}\".", saved.name), json!({"saved_query": saved})))
    }

    async fn write_editor(&self, args: &Value) -> Result<ToolOutput> {
        let sql = arg(args, "sql")?;
        let mode = args.get("mode").and_then(|m| m.as_str()).unwrap_or("replace_selection");
        let proposal = json!({"sql": sql, "mode": mode, "title": args.get("title")});
        let accepted = self.host.propose_edit(&proposal).await;
        self.audit("write_editor", &proposal, if accepted { "approved" } else { "denied" }, None);
        Ok(out(
            if accepted { "The user applied the SQL in the editor." } else { "The user did not apply the change." },
            json!({"proposal": proposal, "accepted": accepted}),
        ))
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n { s.chars().take(n).collect::<String>() + "…" } else { s.to_string() }
}
