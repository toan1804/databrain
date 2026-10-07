//! Running jobs: each step (node) after its upstream steps, independent
//! branches in parallel, with a run record updated as steps finish, and a
//! scheduler that starts jobs whose schedule is due while the app is open.
//!
//! - Query step: its SQL runs on its connection (the Results DuckDB when it
//!   has none) and its last result becomes the output `results.<name>`, so
//!   DuckDB steps downstream can read it.
//! - Load step: its SQL runs on the Results DuckDB (default
//!   `select * from results.<upstream>`), then the rows are written into the
//!   target table of another connection.
//! - Export step: the same, then DuckDB writes the rows to a file
//!   (`COPY … TO`, CSV / Parquet / JSON) in a chosen folder (Downloads by default).
//!
//! Runs don't prompt (the job was set up by hand); read-only connections
//! are still refused.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use databrain_query_engine::{EngineError, EventHub, QueryEngine, Result, RunRequest};
use databrain_workspace::{FileFormat, Job, JobNode, JobNodeKind, JobRun, NodeRunRecord, NodeRunSummary, Origin, Workspace, now_ms};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

/// UI event with a [`JobRunEvent`] payload.
pub const JOB_RUN_EVENT: &str = "job-run";
/// How often the scheduler looks for due jobs.
const SCHEDULER_TICK: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, serde::Serialize)]
pub struct JobRunEvent {
    pub job_id: String,
    pub run: JobRun,
}

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
        },
    }
}

async fn execute(ctx: &JobCtx, job: Job, mut run: JobRun, only: Option<Vec<String>>, cancel: CancellationToken) {
    let selected: HashSet<String> = match &only {
        Some(ids) => ids.iter().cloned().collect(),
        None => job.nodes.iter().map(|n| n.id.clone()).collect(),
    };
    let upstream = |id: &str| -> Vec<String> { job.edges.iter().filter(|e| e.to == id).map(|e| e.from.clone()).collect() };
    // Status per selected step: pending → running → success/error/skipped/cancelled.
    let mut status: HashMap<String, &'static str> = selected.iter().map(|id| (id.clone(), "pending")).collect();
    let mut set = tokio::task::JoinSet::new();
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
                if ups.iter().any(|u| matches!(status.get(u), Some(&"error" | &"skipped" | &"cancelled"))) {
                    status.insert(n.id.clone(), "skipped");
                    run.nodes.retain(|r| r.node_id != n.id);
                    run.nodes.push(record(n, "skipped", None, None, Some("an upstream step failed".into())));
                    changed = true;
                } else if cancel.is_cancelled() {
                    status.insert(n.id.clone(), "cancelled");
                    run.nodes.push(record(n, "cancelled", None, None, None));
                    changed = true;
                } else if ups.iter().all(|u| status.get(u) == Some(&"success")) {
                    status.insert(n.id.clone(), "running");
                    run.nodes.push(record(n, "running", None, None, None));
                    let (c, node, job_id, job_name, cancel) = (ctx.clone(), n.clone(), job.id.clone(), job.name.clone(), cancel.child_token());
                    let ups_names: Vec<String> = upstream(&n.id).iter().filter_map(|u| job.nodes.iter().find(|x| &x.id == u).map(|x| x.name.clone())).collect();
                    set.spawn(async move {
                        let started = Instant::now();
                        let out = run_node(&c, &job_id, &job_name, &node, &ups_names, &cancel).await;
                        (node, started, out)
                    });
                    changed = true;
                }
            }
        }
        let _ = ctx.workspace.update_job_run(&run);
        emit(ctx, &run);
        let Some(done) = set.join_next().await else { break };
        let Ok((node, started, out)) = done else { continue };
        let rec = match out {
            Ok((rows, file)) => {
                status.insert(node.id.clone(), "success");
                let mut r = record(&node, "success", Some(started), Some(rows), None);
                r.summary.file = file;
                r
            }
            Err(e) if cancel.is_cancelled() || e.kind == "cancelled" => {
                status.insert(node.id.clone(), "cancelled");
                record(&node, "cancelled", Some(started), None, Some(e.message))
            }
            Err(e) => {
                status.insert(node.id.clone(), "error");
                record(&node, "error", Some(started), None, Some(e.message))
            }
        };
        if let Some(r) = run.nodes.iter_mut().find(|r| r.node_id == node.id) {
            *r = rec;
        }
    }
    for n in &job.nodes {
        ctx.engine.release_sessions(&node_tab(&job.id, &n.id));
        ctx.engine.release_sessions(&format!("{}:load", node_tab(&job.id, &n.id)));
        ctx.engine.release_sessions(&format!("{}:export", node_tab(&job.id, &n.id)));
    }
    let any = |s: &str| status.values().any(|v| *v == s);
    run.status = if any("error") || any("skipped") {
        "error"
    } else if any("cancelled") || cancel.is_cancelled() {
        "cancelled"
    } else {
        "success"
    }
    .into();
    run.error = run.nodes.iter().find(|r| r.summary.status == "error").map(|r| format!("{}: {}", r.name, r.summary.error.clone().unwrap_or_default()));
    run.finished_at = Some(now_ms());
    let _ = ctx.workspace.update_job_run(&run);
    let _ = ctx.workspace.set_job_node_results(&job.id, &run.nodes);
    emit(ctx, &run);
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

/// One SQL run on a run key (no prompt); fails with the first error.
async fn run_sql(ctx: &JobCtx, connection_id: String, tab: String, sql: String, output_name: Option<String>, cancel: &CancellationToken) -> Result<Vec<databrain_query_engine::StatementOutcomeView>> {
    let outcomes = ctx
        .engine
        .run_and_wait(
            &ctx.hub,
            RunRequest { connection_id, tab_id: tab, sql, base_offset: 0, row_limit: None, confirmed: true, origin: Origin::Job, session_key: None, output_name, params: HashMap::new() },
            Some(cancel.clone()),
        )
        .await?;
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

/// Run one step; rows produced (Query) or written (Load, Export), and the
/// file written (Export).
async fn run_node(ctx: &JobCtx, job_id: &str, job_name: &str, node: &JobNode, upstreams: &[String], cancel: &CancellationToken) -> Result<(i64, Option<String>)> {
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
    let outcomes = run_sql(ctx, connection_id, tab.clone(), sql, Some(node.name.clone()), cancel).await?;
    let last = outcomes.last();
    let rows = last.and_then(|o| o.result.as_ref().map(|r| r.total_rows as i64).or(o.rows_affected.map(|r| r as i64))).unwrap_or(0);
    if node.kind == JobNodeKind::Query {
        return Ok((rows, None));
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
        run_sql(ctx, duck_id, format!("{tab}:export"), copy_sql(&select, &path, node.export_format), None, cancel).await?;
        return Ok((output.rows as i64, Some(path.to_string_lossy().into_owned())));
    }
    let target = node.target_connection_id.as_deref().filter(|c| !c.is_empty()).ok_or_else(|| EngineError::new("invalid", "Choose the connection to load into"))?;
    let table = node.target_table.as_deref().unwrap_or("");
    let written = ctx.engine.write_output(&output.handle, target, &format!("{tab}:load"), table, node.load_mode, cancel).await?;
    Ok((written as i64, None))
}

/// Start due scheduled jobs every [`SCHEDULER_TICK`] while the app runs.
/// A run missed while the app was closed runs once at the next start.
pub fn spawn_scheduler(ctx: JobCtx) {
    let _ = ctx.workspace.close_stale_job_runs();
    tokio::spawn(async move {
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
