//! Running jobs: each step (node) after its upstream steps, independent
//! branches in parallel, with a run record updated as steps finish, and a
//! scheduler that starts jobs whose schedule is due while the app is open.
//!
//! - Query step: its SQL runs on its connection (the Results DuckDB when it
//!   has none) and its last result becomes the output `results.<name>`, so
//!   DuckDB steps downstream can read it.
//! - Load step: its SQL runs on the Results DuckDB (default
//!   `select * from results.<upstream>`), then the rows are written into the
//!   target table of another connection, between its before and after SQL
//!   (see `databrain_query_engine::load`). [`dry_run`] checks one without
//!   keeping any change.
//! - Export step: the same, then DuckDB writes the rows to a file
//!   (`COPY … TO`, CSV / Parquet / JSON) in a chosen folder (Downloads by default).
//!
//! Runs don't prompt (the job was set up by hand); read-only connections
//! are still refused.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use databrain_query_engine::{EngineError, EventHub, LoadDryRun, LoadEvent, LoadObserver, LoadSpec, QueryEngine, Result, RunRequest};
use databrain_workspace::jobs::LOG_KEPT;
use databrain_workspace::{FileFormat, Job, JobNode, JobNodeKind, JobRun, NodeRunRecord, NodeRunSummary, Origin, RunLogEntry, StepProgress, Workspace, now_ms};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

/// UI event with a [`JobRunEvent`] payload.
pub const JOB_RUN_EVENT: &str = "job-run";
/// How often the scheduler looks for due jobs.
const SCHEDULER_TICK: Duration = Duration::from_secs(15);
/// Wait before the first pass after launch.
const STARTUP_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, serde::Serialize)]
pub struct JobRunEvent {
    pub job_id: String,
    pub run: JobRun,
}

/// UI event with a [`JobLogEvent`] payload: new lines of a run's log.
pub const JOB_LOG_EVENT: &str = "job-run-log";

#[derive(Debug, Clone, serde::Serialize)]
pub struct JobLogEvent {
    pub job_id: String,
    pub run_id: i64,
    pub entries: Vec<RunLogEntry>,
}

/// Progress events are sent at most this often per run.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

/// Jobs running now, cancellable by job id.
#[derive(Default)]
pub struct JobsState {
    running: Mutex<HashMap<String, CancellationToken>>,
}

impl JobsState {
    pub fn is_running(&self, job_id: &str) -> bool {
        self.running.lock().contains_key(job_id)
    }
    pub fn running_ids(&self) -> Vec<String> {
        self.running.lock().keys().cloned().collect()
    }
    pub fn cancel(&self, job_id: &str) -> bool {
        match self.running.lock().get(job_id) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }
}

/// Everything a run needs, cheap to clone into tasks.
#[derive(Clone)]
pub struct JobCtx {
    pub engine: Arc<QueryEngine>,
    pub workspace: Arc<Workspace>,
    pub hub: Arc<EventHub>,
    pub ui: Arc<dyn crate::ai_api::UiBridge>,
    pub jobs: Arc<JobsState>,
}

/// Session/run key of a step.
fn node_tab(job: &str, node: &str) -> String {
    format!("job:{job}:{node}")
}

fn emit(ctx: &JobCtx, run: &JobRun) {
    let ev = JobRunEvent { job_id: run.job_id.clone(), run: run.clone() };
    ctx.ui.emit(JOB_RUN_EVENT, serde_json::to_value(ev).unwrap_or_default());
}

/// A run being executed: its record, its log, and when it was last sent.
struct Live {
    run: JobRun,
    log: Vec<RunLogEntry>,
    /// Lines dropped past [`LOG_KEPT`].
    dropped: usize,
    last_emit: Instant,
}

/// Writes a run's log and progress; cloned into each step (with its node).
#[derive(Clone)]
pub struct Tracer {
    ctx: JobCtx,
    live: Arc<Mutex<Live>>,
    node: Option<(String, String)>,
}

impl Tracer {
    fn new(ctx: &JobCtx, run: JobRun) -> Self {
        Tracer { ctx: ctx.clone(), live: Arc::new(Mutex::new(Live { run, log: Vec::new(), dropped: 0, last_emit: Instant::now() })), node: None }
    }

    fn for_node(&self, node: &JobNode) -> Self {
        Tracer { node: Some((node.id.clone(), node.name.clone())), ..self.clone() }
    }

    /// Add a line to the log (and send it to the UI).
    pub fn log(&self, level: &str, message: impl Into<String>, sql: Option<&str>, rows: Option<i64>, duration_ms: Option<i64>) {
        let (run_id, entry) = {
            let mut l = self.live.lock();
            if l.log.len() >= LOG_KEPT {
                l.dropped += 1;
                return;
            }
            let entry = RunLogEntry {
                seq: l.log.len() as u64 + 1,
                at: now_ms(),
                node_id: self.node.as_ref().map(|n| n.0.clone()),
                step: self.node.as_ref().map(|n| n.1.clone()),
                level: level.into(),
                message: message.into(),
                sql: sql.map(databrain_workspace::jobs::log_sql),
                rows,
                duration_ms,
            };
            l.log.push(entry.clone());
            (l.run.id, entry)
        };
        let ev = JobLogEvent { job_id: self.job_id(), run_id, entries: vec![entry] };
        self.ctx.ui.emit(JOB_LOG_EVENT, serde_json::to_value(ev).unwrap_or_default());
    }

    fn job_id(&self) -> String {
        self.live.lock().run.job_id.clone()
    }

    /// Set this step's progress; the run is sent at most every [`PROGRESS_EVERY`]
    /// (always when the work is done).
    fn progress(&self, p: StepProgress) {
        let Some((id, _)) = &self.node else { return };
        let snapshot = {
            let mut l = self.live.lock();
            let finished = p.done >= p.total;
            if let Some(r) = l.run.nodes.iter_mut().find(|r| &r.node_id == id) {
                r.summary.progress = Some(p);
            }
            if !finished && l.last_emit.elapsed() < PROGRESS_EVERY {
                return;
            }
            l.last_emit = Instant::now();
            l.run.clone()
        };
        emit(&self.ctx, &snapshot);
    }

    fn with_run<T>(&self, f: impl FnOnce(&mut JobRun) -> T) -> T {
        f(&mut self.live.lock().run)
    }

    /// Save the run and its log, and send the run to the UI.
    fn save(&self) {
        let (run, log) = {
            let mut l = self.live.lock();
            l.last_emit = Instant::now();
            let mut log = l.log.clone();
            if l.dropped > 0 {
                log.push(RunLogEntry { seq: log.len() as u64 + 1, at: now_ms(), level: "warning".into(), message: format!("{} more lines were not kept (the log keeps {LOG_KEPT})", l.dropped), ..Default::default() });
            }
            (l.run.clone(), log)
        };
        let _ = self.ctx.workspace.update_job_run(&run);
        let _ = self.ctx.workspace.set_job_run_log(run.id, &log);
        emit(&self.ctx, &run);
    }
}

impl LoadObserver for Tracer {
    fn event(&self, e: LoadEvent) {
        match e {
            LoadEvent::Info(m) => self.log("info", m, None, None, None),
            LoadEvent::Statement { label, sql, rows, duration_ms, error } => match error {
                None => self.log("info", format!("{label}: done{}", rows.map(|r| format!(", {r} rows affected")).unwrap_or_default()), Some(&sql), rows.map(|r| r as i64), Some(duration_ms as i64)),
                Some(e) => self.log("error", format!("{label} failed: {e}"), Some(&sql), None, Some(duration_ms as i64)),
            },
            LoadEvent::Progress { phase, done, total } => self.progress(StepProgress { phase, done, total }),
            LoadEvent::Inserted { label, table, rows, statements, sql, duration_ms, error } => {
                let batches = format!("{statements} batch{}", if statements == 1 { "" } else { "es" });
                match error {
                    None => self.log("info", format!("{label}: {rows} rows into {table} in {batches}"), (!sql.is_empty()).then_some(sql.as_str()), Some(rows as i64), Some(duration_ms as i64)),
                    Some(e) => self.log("error", format!("{label} failed after {rows} rows ({batches}): {e}"), Some(&sql), Some(rows as i64), Some(duration_ms as i64)),
                }
            }
        }
    }
}

/// Start a run in the background. `only`: run just these steps (their
/// upstream outputs must already exist); `None` runs the whole job.
pub fn start(ctx: &JobCtx, job_id: &str, trigger: &str, only: Option<Vec<String>>) -> Result<JobRun> {
    let job = ctx.workspace.get_job(job_id)?;
    if job.nodes.is_empty() {
        return Err(EngineError::new("invalid", "This job has no steps"));
    }
    let cancel = CancellationToken::new();
    {
        let mut running = ctx.jobs.running.lock();
        if running.contains_key(job_id) {
            return Err(EngineError::new("busy", format!("\"{}\" is already running", job.name)));
        }
        running.insert(job_id.to_string(), cancel.clone());
    }
    let run = match ctx.workspace.start_job_run(job_id, trigger) {
        Ok(r) => r,
        Err(e) => {
            ctx.jobs.running.lock().remove(job_id);
            return Err(e.into());
        }
    };
    emit(ctx, &run);
    let c = ctx.clone();
    let r = run.clone();
    tokio::spawn(async move {
        let id = r.job_id.clone();
        execute(&c, job, r, only, cancel).await;
        c.jobs.running.lock().remove(&id);
    });
    Ok(run)
}

fn record(node: &JobNode, status: &str, started: Option<Instant>, rows: Option<i64>, error: Option<String>) -> NodeRunRecord {
    NodeRunRecord {
        node_id: node.id.clone(),
        name: node.name.clone(),
        summary: NodeRunSummary {
            status: status.into(),
            finished_at: if status == "running" { 0 } else { now_ms() },
            duration_ms: started.map(|s| s.elapsed().as_millis() as i64).unwrap_or(0),
            rows,
            error,
            file: None,
            notices: vec![],
            progress: None,
        },
    }
}

fn action_text(node: &JobNode) -> &'static str {
    match node.kind {
        JobNodeKind::Query => "query",
        JobNodeKind::Load => "load into a connection",
        JobNodeKind::Export => "save to a file",
    }
}

async fn execute(ctx: &JobCtx, job: Job, run: JobRun, only: Option<Vec<String>>, cancel: CancellationToken) {
    let selected: HashSet<String> = match &only {
        Some(ids) => ids.iter().cloned().collect(),
        None => job.nodes.iter().map(|n| n.id.clone()).collect(),
    };
    let tracer = Tracer::new(ctx, run.clone());
    let trigger = if run.trigger == "schedule" { "on its schedule" } else { "by hand" };
    tracer.log(
        "info",
        match &only {
            Some(_) => format!("Run of \"{}\" started {trigger}: {} of {} steps", job.name, selected.len(), job.nodes.len()),
            None => format!("Run of \"{}\" started {trigger}: {} steps", job.name, job.nodes.len()),
        },
        None,
        None,
        None,
    );
    let upstream = |id: &str| -> Vec<String> { job.edges.iter().filter(|e| e.to == id).map(|e| e.from.clone()).collect() };
    // Status per selected step: pending → running → success/error/skipped/cancelled.
    let mut status: HashMap<String, &'static str> = selected.iter().map(|id| (id.clone(), "pending")).collect();
    let mut set = tokio::task::JoinSet::new();
    let started_run = Instant::now();
    loop {
        // Start every step whose selected upstreams succeeded; skip those after a failure.
        let mut changed = true;
        while changed {
            changed = false;
            for n in &job.nodes {
                if status.get(&n.id) != Some(&"pending") {
                    continue;
                }
                let ups: Vec<String> = upstream(&n.id).into_iter().filter(|u| selected.contains(u)).collect();
                let t = tracer.for_node(n);
                if let Some(failed) = ups.iter().find(|u| matches!(status.get(*u), Some(&"error" | &"skipped" | &"cancelled"))) {
                    status.insert(n.id.clone(), "skipped");
                    let name = job.nodes.iter().find(|x| &x.id == failed).map(|x| x.name.clone()).unwrap_or_default();
                    t.log("warning", format!("Skipped: upstream step {name} did not succeed"), None, None, None);
                    t.with_run(|run| {
                        run.nodes.retain(|r| r.node_id != n.id);
                        run.nodes.push(record(n, "skipped", None, None, Some("an upstream step failed".into())));
                    });
                    changed = true;
                } else if cancel.is_cancelled() {
                    status.insert(n.id.clone(), "cancelled");
                    t.log("warning", "Not started: the run was stopped", None, None, None);
                    t.with_run(|run| run.nodes.push(record(n, "cancelled", None, None, None)));
                    changed = true;
                } else if ups.iter().all(|u| status.get(u) == Some(&"success")) {
                    status.insert(n.id.clone(), "running");
                    t.log("info", format!("Started ({})", action_text(n)), None, None, None);
                    t.with_run(|run| run.nodes.push(record(n, "running", None, None, None)));
                    let (c, node, job_id, job_name, cancel) = (ctx.clone(), n.clone(), job.id.clone(), job.name.clone(), cancel.child_token());
                    let ups_names: Vec<String> = upstream(&n.id).iter().filter_map(|u| job.nodes.iter().find(|x| &x.id == u).map(|x| x.name.clone())).collect();
                    set.spawn(async move {
                        let started = Instant::now();
                        let out = run_node(&c, &job_id, &job_name, &node, &ups_names, &cancel, &t).await;
                        (node, started, out, t)
                    });
                    changed = true;
                }
            }
        }
        tracer.save();
        let Some(done) = set.join_next().await else { break };
        let Ok((node, started, out, t)) = done else { continue };
        let took = started.elapsed().as_millis() as i64;
        let rec = match out {
            Ok(out) => {
                status.insert(node.id.clone(), "success");
                t.log("success", format!("Succeeded: {} rows{}", out.rows, out.file.as_ref().map(|f| format!(", file {f}")).unwrap_or_default()), None, Some(out.rows), Some(took));
                let mut r = record(&node, "success", Some(started), Some(out.rows), None);
                r.summary.file = out.file;
                r.summary.notices = out.notices;
                r
            }
            Err(e) if cancel.is_cancelled() || e.kind == "cancelled" => {
                status.insert(node.id.clone(), "cancelled");
                t.log("warning", "Cancelled", None, None, Some(took));
                record(&node, "cancelled", Some(started), None, Some(e.message))
            }
            Err(e) => {
                status.insert(node.id.clone(), "error");
                t.log("error", format!("Failed: {}", e.message), None, None, Some(took));
                record(&node, "error", Some(started), None, Some(e.message))
            }
        };
        tracer.with_run(|run| {
            if let Some(r) = run.nodes.iter_mut().find(|r| r.node_id == node.id) {
                *r = rec;
            }
        });
    }
    for n in &job.nodes {
        ctx.engine.release_sessions(&node_tab(&job.id, &n.id));
        ctx.engine.release_sessions(&format!("{}:load", node_tab(&job.id, &n.id)));
        ctx.engine.release_sessions(&format!("{}:export", node_tab(&job.id, &n.id)));
    }
    let any = |s: &str| status.values().any(|v| *v == s);
    let final_status = if any("error") || any("skipped") {
        "error"
    } else if any("cancelled") || cancel.is_cancelled() {
        "cancelled"
    } else {
        "success"
    };
    let nodes = tracer.with_run(|run| {
        run.status = final_status.into();
        run.error = run.nodes.iter().find(|r| r.summary.status == "error").map(|r| format!("{}: {}", r.name, r.summary.error.clone().unwrap_or_default()));
        run.finished_at = Some(now_ms());
        run.nodes.clone()
    });
    let count = |s: &str| nodes.iter().filter(|r| r.summary.status == s).count();
    tracer.log(
        match final_status {
            "success" => "success",
            "cancelled" => "warning",
            _ => "error",
        },
        format!(
            "Run {}: {} succeeded, {} failed, {} skipped, {} cancelled",
            match final_status {
                "success" => "succeeded",
                "cancelled" => "was stopped",
                _ => "failed",
            },
            count("success"),
            count("error"),
            count("skipped"),
            count("cancelled")
        ),
        None,
        None,
        Some(started_run.elapsed().as_millis() as i64),
    );
    let _ = ctx.workspace.set_job_node_results(&job.id, &nodes);
    tracer.save();
}

/// SQL a load step runs when it has none: its upstream's output.
pub fn default_load_sql(upstreams: &[String]) -> Option<String> {
    upstreams.first().map(|u| format!("select * from results.{}", databrain_query_engine::outputs::q_min(u)))
}

/// Folder an export step writes to: its own, else the Downloads folder.
pub fn export_folder(node: &JobNode) -> Result<std::path::PathBuf> {
    let dir = match node.export_folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => std::path::PathBuf::from(f),
        None => dirs::download_dir().or_else(|| dirs::home_dir().map(|h| h.join("Downloads"))).ok_or_else(|| EngineError::new("invalid", "Choose the folder to save the file in"))?,
    };
    if !dir.is_absolute() {
        return Err(EngineError::new("invalid", format!("The folder must be a full path: {}", dir.display())));
    }
    if !dir.is_dir() {
        return Err(EngineError::new("invalid", format!("Folder not found: {}", dir.display())));
    }
    Ok(dir)
}

/// DuckDB statement writing `select` to `path`.
pub fn copy_sql(select: &str, path: &std::path::Path, format: FileFormat) -> String {
    let opts = match format {
        FileFormat::Csv => "FORMAT csv, HEADER true",
        FileFormat::Parquet => "FORMAT parquet, COMPRESSION zstd",
        FileFormat::Json => "FORMAT json",
    };
    format!("COPY ({select}) TO {} ({opts})", databrain_connector_core::quote_literal(&path.to_string_lossy()))
}

/// One SQL run on a run key (no prompt); fails with the first error. Each
/// statement's outcome goes to the log.
async fn run_sql(ctx: &JobCtx, connection_id: String, tab: String, sql: String, output_name: Option<String>, cancel: &CancellationToken, t: &Tracer) -> Result<Vec<databrain_query_engine::StatementOutcomeView>> {
    let conn = ctx.workspace.get_connection(&connection_id).map(|p| p.name).unwrap_or_else(|_| connection_id.clone());
    t.log("info", format!("Running SQL on {conn}"), None, None, None);
    let outcomes = ctx
        .engine
        .run_and_wait(
            &ctx.hub,
            RunRequest { connection_id, tab_id: tab, sql, base_offset: 0, row_limit: None, confirmed: true, origin: Origin::Job, session_key: None, output_name, params: HashMap::new() },
            Some(cancel.clone()),
        )
        .await?;
    let n = outcomes.len();
    for o in &outcomes {
        let which = if n > 1 { format!("Statement {} of {n}", o.index + 1) } else { "Statement".to_string() };
        for notice in &o.notices {
            t.log("info", format!("{which}: {notice}"), None, None, None);
        }
        match (&o.error, &o.result, o.rows_affected) {
            (Some(e), _, _) => t.log("error", format!("{which} failed: {}", e.message), Some(&o.sql), None, Some(o.duration_ms as i64)),
            (None, Some(r), _) => t.log(
                "info",
                format!("{which}: {} rows, {} columns{}", r.total_rows, r.columns.len(), if r.truncated { " (capped)" } else { "" }),
                Some(&o.sql),
                Some(r.total_rows as i64),
                Some(o.duration_ms as i64),
            ),
            (None, None, rows) => t.log("info", format!("{which}: done{}", rows.map(|r| format!(", {r} rows affected")).unwrap_or_default()), Some(&o.sql), rows.map(|r| r as i64), Some(o.duration_ms as i64)),
        }
    }
    if let Some((i, e)) = outcomes.iter().enumerate().find_map(|(i, o)| o.error.as_ref().map(|e| (i, e))) {
        let mut e = e.clone();
        if outcomes.len() > 1 {
            e.message = format!("statement {}: {}", i + 1, e.message);
        }
        return Err(e);
    }
    if cancel.is_cancelled() {
        return Err(EngineError::new("cancelled", "Cancelled"));
    }
    Ok(outcomes)
}

/// What a step did.
struct NodeOut {
    /// Rows produced (Query) or written (Load, Export).
    rows: i64,
    /// File written (Export).
    file: Option<String>,
    notices: Vec<String>,
}

/// Run one step.
async fn run_node(ctx: &JobCtx, job_id: &str, job_name: &str, node: &JobNode, upstreams: &[String], cancel: &CancellationToken, t: &Tracer) -> Result<NodeOut> {
    let tab = node_tab(job_id, &node.id);
    let duck = || ctx.engine.results_connection().map(|c| c.id);
    let (connection_id, sql) = match node.kind {
        JobNodeKind::Query => (match &node.connection_id {
            Some(c) if !c.is_empty() => c.clone(),
            _ => duck()?,
        }, node.sql.clone()),
        JobNodeKind::Load | JobNodeKind::Export => {
            let sql = if node.sql.trim().is_empty() {
                default_load_sql(upstreams).ok_or_else(|| EngineError::new("invalid", "Write the SQL that selects the rows to load, or connect an upstream step"))?
            } else {
                node.sql.clone()
            };
            (duck()?, sql)
        }
    };
    if sql.trim().is_empty() {
        return Err(EngineError::new("invalid", "This step has no SQL"));
    }
    // Checked before the query runs, so a missing folder fails fast.
    let folder = if node.kind == JobNodeKind::Export { Some(export_folder(node)?) } else { None };
    let duck_id = connection_id.clone();
    let outcomes = run_sql(ctx, connection_id, tab.clone(), sql, Some(node.name.clone()), cancel, t).await?;
    let last = outcomes.last();
    let rows = last.and_then(|o| o.result.as_ref().map(|r| r.total_rows as i64).or(o.rows_affected.map(|r| r as i64))).unwrap_or(0);
    if node.kind == JobNodeKind::Query {
        return Ok(NodeOut { rows, file: None, notices: vec![] });
    }
    let output = ctx
        .engine
        .outputs()
        .for_tab(&tab)
        .into_iter()
        .rev()
        .find(|o| o.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&node.name)))
        .ok_or_else(|| EngineError::new("invalid", "The SQL returned no rows (it must end with a SELECT)"))?;
    if let Some(folder) = folder {
        let name = databrain_workspace::jobs::export_file_name(node.export_file.as_deref(), &node.name, job_name, node.export_format, now_ms());
        let path = folder.join(name);
        // By handle: exactly this run's rows, even if another step has the same name.
        let select = format!("select * from results.{}", output.handle);
        t.log("info", format!("Writing {} rows to {}", output.rows, path.display()), None, None, None);
        run_sql(ctx, duck_id, format!("{tab}:export"), copy_sql(&select, &path, node.export_format), None, cancel, t).await?;
        return Ok(NodeOut { rows: output.rows as i64, file: Some(path.to_string_lossy().into_owned()), notices: vec![] });
    }
    let target = node.target_connection_id.as_deref().filter(|c| !c.is_empty()).ok_or_else(|| EngineError::new("invalid", "Choose the connection to load into"))?;
    let report = ctx.engine.write_output(&output.handle, target, &format!("{tab}:load"), &load_spec(node), cancel, t).await?;
    Ok(NodeOut { rows: report.rows as i64, file: None, notices: report.notices })
}

/// What a load step writes, from its settings.
pub fn load_spec(node: &JobNode) -> LoadSpec {
    let text = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    LoadSpec {
        table: node.target_table.clone().unwrap_or_default(),
        mode: node.load_mode,
        key_columns: node.key_columns.clone(),
        create_table: node.create_table,
        before_sql: text(&node.load_before_sql),
        after_sql: text(&node.load_after_sql),
        batch_rows: node.batch_rows,
        batch_kb: node.batch_kb,
    }
}

/// Rows a dry run selects to try the load with.
pub const DRY_RUN_ROWS: usize = 1000;

/// Check a load step of `job` (as edited, not necessarily saved) without
/// keeping any change: its SQL selects up to [`DRY_RUN_ROWS`] rows on
/// DuckDB, then the load is tried with them (see
/// [`QueryEngine::dry_run_load`]).
pub async fn dry_run(ctx: &JobCtx, job: &Job, node_id: &str) -> Result<LoadDryRun> {
    let node = job.nodes.iter().find(|n| n.id == node_id).ok_or_else(|| EngineError::new("not_found", "Step not found"))?;
    if node.kind != JobNodeKind::Load {
        return Err(EngineError::new("invalid", "Only load steps have a dry run"));
    }
    let target = node.target_connection_id.as_deref().filter(|c| !c.is_empty()).ok_or_else(|| EngineError::new("invalid", "Choose the connection to load into"))?;
    let ups: Vec<String> = job.edges.iter().filter(|e| e.to == node.id).filter_map(|e| job.nodes.iter().find(|n| n.id == e.from).map(|n| n.name.clone())).collect();
    let sql = if node.sql.trim().is_empty() { default_load_sql(&ups) } else { Some(node.sql.clone()) };
    // Not a `job:` tab: its result is not the step's output.
    let tab = format!("dryrun:{}:{}", job.id, node.id);
    let cancel = CancellationToken::new();
    let (rows, rows_error) = match sql {
        None => (None, Some("Write the SQL that selects the rows to load, or connect an upstream step".to_string())),
        Some(sql) => {
            let duck = ctx.engine.results_connection()?.id;
            let req = RunRequest { connection_id: duck, tab_id: tab.clone(), sql, base_offset: 0, row_limit: Some(DRY_RUN_ROWS), confirmed: true, origin: Origin::Job, session_key: None, output_name: None, params: HashMap::new() };
            match ctx.engine.run_and_wait(&ctx.hub, req, Some(cancel.clone())).await {
                Err(e) => (None, Some(e.message)),
                Ok(outcomes) => match outcomes.iter().find_map(|o| o.error.as_ref()) {
                    Some(e) => (None, Some(format!("{}{}", e.message, if ups.is_empty() { "" } else { " (run the upstream steps first so their outputs exist)" }))),
                    None => match outcomes.last().and_then(|o| o.result.as_ref()) {
                        None => (None, Some("The SQL returned no rows (it must end with a SELECT)".to_string())),
                        Some(r) => match ctx.engine.result_rows(&r.id) {
                            Ok(rows) => (Some(rows), None),
                            Err(e) => (None, Some(e.message)),
                        },
                    },
                },
            }
        }
    };
    let report = ctx.engine.dry_run_load(rows.as_ref(), rows_error, target, &format!("{tab}:load"), &load_spec(node)).await;
    ctx.engine.close_tab(&tab);
    ctx.engine.release_sessions(&format!("{tab}:load"));
    Ok(report)
}

/// Start due scheduled jobs every [`SCHEDULER_TICK`] while the app runs.
/// A run missed while the app was closed runs once at the next start.
pub fn spawn_scheduler(ctx: JobCtx) {
    let _ = ctx.workspace.close_stale_job_runs();
    tokio::spawn(async move {
        // Let the window load and listen first, so runs started at launch
        // are announced ("Starting scheduled job …").
        tokio::time::sleep(STARTUP_DELAY).await;
        loop {
            tick(&ctx);
            tokio::time::sleep(SCHEDULER_TICK).await;
        }
    });
}

/// One scheduler pass; returns the jobs started.
pub fn tick(ctx: &JobCtx) -> Vec<String> {
    let now = now_ms();
    let mut started = Vec::new();
    for j in ctx.workspace.list_jobs().unwrap_or_default() {
        if !j.schedule.enabled || ctx.jobs.is_running(&j.id) || j.next_run_at.is_none_or(|t| t > now) {
            continue;
        }
        let _ = ctx.workspace.mark_job_scheduled(&j.id, now);
        if start(ctx, &j.id, "schedule", None).is_ok() {
            started.push(j.id);
        }
    }
    started
}
