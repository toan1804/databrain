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
//! Limits ([`databrain_workspace::RunLimits`]): a run and each try of a
//! step may have a time limit (then they are stopped and fail); a step that
//! lost its connection is tried again after a growing wait when that can't
//! write twice (reads, exports, a load rolled back as a whole); at most so
//! many steps run at once, and so many on one connection (the rest wait).
//!
//! Runs don't prompt (the job was set up by hand, or approved when the AI
//! assistant built or ran it: the assistant's policy is checked then);
//! read-only connections are still refused. SQL the assistant wrote
//! (`JobNode::ai_sql`), and every step of a run the assistant starts, runs
//! as its own queries, whoever starts the run: each step's SQL, a load's
//! before/after SQL and its write (the DuckDB sandbox on DuckDB targets),
//! after the connection's AI rules as they are now. SQL DataBrain writes
//! for a step (an export's COPY, a load's INSERTs) is the job's.

use futures::FutureExt;
use std::collections::{BTreeMap, HashMap, HashSet};

use databrain_connector_core::sql::ParamValue;
use std::sync::Arc;
use std::time::{Duration, Instant};

use databrain_query_engine::{EngineError, EventHub, LoadDryRun, LoadEvent, LoadObserver, LoadSpec, QueryEngine, Result, RunRequest};
use databrain_workspace::jobs::LOG_KEPT;
use databrain_workspace::{
    FileFormat, Job, JobNode, JobNodeKind, JobRun, NodeRunRecord, NodeRunSummary, Origin, RunLogEntry, StepProgress, Workspace, duration_text, now_ms,
};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

/// UI event with a [`JobRunEvent`] payload.
pub const JOB_RUN_EVENT: &str = "job-run";
/// How often the scheduler looks for due jobs.
const SCHEDULER_TICK: Duration = Duration::from_secs(15);
/// Wait before the first pass after launch.
const STARTUP_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, serde::Serialize, ts_rs::TS)]
#[ts(export)]
pub struct JobRunEvent {
    pub job_id: String,
    pub run: JobRun,
}

/// UI event with a [`JobLogEvent`] payload: new lines of a run's log.
pub const JOB_LOG_EVENT: &str = "job-run-log";

#[derive(Debug, Clone, serde::Serialize, ts_rs::TS)]
#[ts(export)]
pub struct JobLogEvent {
    pub job_id: String,
    #[ts(as = "f64")]
    pub run_id: i64,
    pub entries: Vec<RunLogEntry>,
}

/// Progress events are sent at most this often per run.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

/// Logged when another runner holds the job now.
const LEASE_LOST: &str = "Stopped: another runner took this job (its lease ran out, e.g. while the computer slept); it runs there, not twice";

/// Renew `owner`'s lease of `job_id`: `Some(true)` held, `Some(false)`
/// another runner holds it, `None` the workspace kept failing (a few
/// tries; a busy or locked database is not a lost lease).
async fn renew_lease(ws: &Workspace, job_id: &str, owner: &str) -> Option<bool> {
    for attempt in 0..3 {
        match ws.claim_job(job_id, owner, now_ms() + LEASE_MS) {
            Ok(held) => return Some(held),
            Err(e) => {
                tracing::warn!(job = job_id, attempt, error = %e, "job lease not renewed (workspace error), trying again");
                tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
            }
        }
    }
    None
}

/// After a run or a step is stopped (by the user, a time limit, a lost
/// lease), how long its work has to end before it is left behind: a
/// statement the database doesn't let go of can't keep the run (and its
/// lease) alive.
const STOP_GRACE: Duration = Duration::from_secs(30);
/// A step whose earlier try, left behind, still runs.
const STILL_RUNNING: &str = "An earlier try of this step is still running (it didn't stop when asked and was left behind): wait for it to end, or stop it on the server, then run the step again";

/// How long a job's lease lasts; renewed every [`LEASE_RENEW`] while it runs.
const LEASE_MS: i64 = 90_000;
const LEASE_RENEW: Duration = Duration::from_secs(20);

/// Jobs running now, cancellable by job id.
pub struct JobsState {
    running: Mutex<HashMap<String, CancellationToken>>,
    /// Steps running now, by (job id, step id): stop one step only.
    steps: Mutex<HashMap<(String, String), CancellationToken>>,
    /// This process as a job runner (`app:<pid>:<n>` or `cli:<pid>:<n>`):
    /// a job runs in one process at a time (lease in the workspace).
    pub owner: String,
    /// Wait before a step's first retry (ms; see [`databrain_workspace::retry_delay`]).
    retry_base_ms: std::sync::atomic::AtomicU64,
}

impl Default for JobsState {
    fn default() -> Self {
        Self::new("app")
    }
}

impl JobsState {
    /// `kind`: `app` (the desktop app) or `cli` (the background runner).
    pub fn new(kind: &str) -> Self {
        let n = uuid::Uuid::new_v4().simple().to_string();
        Self {
            running: Mutex::default(),
            steps: Mutex::default(),
            owner: format!("{kind}:{}:{}", std::process::id(), &n[..8]),
            retry_base_ms: (databrain_workspace::RETRY_DELAY.as_millis() as u64).into(),
        }
    }
    /// Change the wait before a step's first retry (tests).
    pub fn set_retry_delay(&self, d: Duration) {
        self.retry_base_ms.store(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
    }
    fn retry_base(&self) -> Duration {
        Duration::from_millis(self.retry_base_ms.load(std::sync::atomic::Ordering::Relaxed))
    }
    pub fn is_running(&self, job_id: &str) -> bool {
        self.running.lock().contains_key(job_id)
    }
    pub fn running_ids(&self) -> Vec<String> {
        self.running.lock().keys().cloned().collect()
    }
    /// Stop one running step; the rest of the run goes on (steps after it
    /// are cancelled, as it didn't succeed).
    pub fn cancel_step(&self, job_id: &str, node_id: &str) -> bool {
        match self.steps.lock().get(&(job_id.to_string(), node_id.to_string())) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
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
    databrain_workspace::SessionRole::JobStep { job: job.into(), node: node.into() }.key()
}

/// Session of a step's load (writing rows into its target).
fn step_load(step: &str) -> String {
    databrain_workspace::SessionRole::Load(step.into()).key()
}

/// Session of a step's export (DuckDB writing the file).
fn step_export(step: &str) -> String {
    databrain_workspace::SessionRole::Export(step.into()).key()
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

/// `:name` values for one run instead of the job's own: a saved set (id
/// or name) and values over it (command line, "Run with…", a resume).
#[derive(Debug, Clone, Default, serde::Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct RunParams {
    #[serde(default)]
    #[ts(optional)]
    pub set: Option<String>,
    #[serde(default)]
    #[ts(as = "Option<std::collections::BTreeMap<String, ParamValue>>", optional)]
    pub values: BTreeMap<String, ParamValue>,
}

/// The values a run's steps get: the set's (the run's, else the job's),
/// then the job's own, then the run's. Also says where they came from.
pub fn effective_params(ws: &Workspace, job: &Job, over: Option<&RunParams>) -> std::result::Result<(BTreeMap<String, ParamValue>, Option<String>), String> {
    let mut out = BTreeMap::new();
    let mut set_name = None;
    let set = over.and_then(|o| o.set.clone()).filter(|s| !s.trim().is_empty()).or_else(|| job.param_set.clone());
    if let Some(s) = set {
        match ws.find_param_set(&s).map_err(|e| e.to_string())? {
            Some(found) => {
                out.extend(found.values);
                set_name = Some(found.name);
            }
            None => return Err(format!("The parameter set this run uses doesn't exist any more ({s}): choose another in the job's parameters")),
        }
    }
    out.extend(job.params.clone());
    if let Some(o) = over {
        out.extend(o.values.clone());
    }
    Ok((out, set_name))
}

/// Writes a run's log and progress; cloned into each step (with its node).
#[derive(Clone)]
pub struct Tracer {
    ctx: JobCtx,
    live: Arc<Mutex<Live>>,
    node: Option<(String, String)>,
    /// `:name` values of the run (or why there are none: its steps fail with it).
    params: Arc<std::result::Result<HashMap<String, ParamValue>, String>>,
    /// How far the step's load got, for [`retry_refused`].
    load: Arc<Mutex<LoadState>>,
}

impl Tracer {
    fn new(ctx: &JobCtx, run: JobRun) -> Self {
        Tracer {
            ctx: ctx.clone(),
            live: Arc::new(Mutex::new(Live { run, log: Vec::new(), dropped: 0, last_emit: Instant::now() })),
            node: None,
            params: Arc::new(Ok(HashMap::new())),
            load: Arc::default(),
        }
    }

    /// The run's `:name` values for a step's SQL.
    fn params(&self) -> Result<HashMap<String, ParamValue>> {
        self.params.as_ref().clone().map_err(|e| EngineError::new("invalid", e))
    }

    fn for_node(&self, node: &JobNode) -> Self {
        Tracer { node: Some((node.id.clone(), node.name.clone())), load: Arc::default(), ..self.clone() }
    }

    /// Show which try of the step this is ("attempt 2 of 3"), with a fresh start.
    fn attempt(&self, n: u32, of: u32) {
        *self.load.lock() = LoadState::NotStarted;
        let Some((id, _)) = &self.node else { return };
        let snapshot = {
            let mut l = self.live.lock();
            if let Some(r) = l.run.nodes.iter_mut().find(|r| &r.node_id == id) {
                r.summary.attempt = Some(n);
                r.summary.attempts = Some(of);
                r.summary.progress = None;
            }
            l.last_emit = Instant::now();
            l.run.clone()
        };
        emit(&self.ctx, &snapshot);
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

    /// Who a step's own SQL runs for: SQL the AI assistant wrote
    /// (`JobNode::ai_sql`), and every step of a run the assistant started,
    /// runs like the assistant's queries (`Origin::Ai`: in the DuckDB
    /// sandbox unless the connection lets the AI read files and the
    /// network), also on the schedule or by hand. SQL DataBrain writes for
    /// a step (`COPY … TO` an export file) is not the AI's and runs as the
    /// job's.
    fn step_origin(&self, node: &JobNode) -> Origin {
        if node.ai_sql || self.live.lock().run.trigger == "ai" { Origin::Ai } else { Origin::Job }
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
                log.push(RunLogEntry {
                    seq: log.len() as u64 + 1,
                    at: now_ms(),
                    level: "warning".into(),
                    message: format!("{} more lines were not kept (the log keeps {LOG_KEPT})", l.dropped),
                    ..Default::default()
                });
            }
            (l.run.clone(), log)
        };
        let _ = self.ctx.workspace.update_job_run(&run);
        let _ = self.ctx.workspace.set_job_run_log(run.id, &log);
        emit(&self.ctx, &run);
    }
}

/// How far a step's load got in its last try. Unsure is not "kept nothing".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LoadState {
    /// The load wasn't called (the step failed before it).
    #[default]
    NotStarted,
    /// Called, and it reported no failure: it may have written.
    Started,
    /// It failed and said it kept nothing ([`LoadEvent::Failed`]).
    KeptNothing,
    /// It failed and may have kept some of its writes.
    MayHaveKept,
}

impl LoadObserver for Tracer {
    fn event(&self, e: LoadEvent) {
        match e {
            LoadEvent::Info(m) => self.log("info", m, None, None, None),
            LoadEvent::Statement { label, sql, rows, duration_ms, error } => match error {
                None => self.log(
                    "info",
                    format!("{label}: done{}", rows.map(|r| format!(", {r} rows affected")).unwrap_or_default()),
                    Some(&sql),
                    rows.map(|r| r as i64),
                    Some(duration_ms as i64),
                ),
                Some(e) => self.log("error", format!("{label} failed: {e}"), Some(&sql), None, Some(duration_ms as i64)),
            },
            LoadEvent::Progress { phase, done, total } => self.progress(StepProgress { phase, done, total }),
            LoadEvent::Failed { kept_nothing } => *self.load.lock() = if kept_nothing { LoadState::KeptNothing } else { LoadState::MayHaveKept },
            LoadEvent::Inserted { label, table, rows, statements, sql, duration_ms, error } => {
                let batches = format!("{statements} batch{}", if statements == 1 { "" } else { "es" });
                match error {
                    None => self.log(
                        "info",
                        format!("{label}: {rows} rows into {table} in {batches}"),
                        (!sql.is_empty()).then_some(sql.as_str()),
                        Some(rows as i64),
                        Some(duration_ms as i64),
                    ),
                    Some(e) => {
                        self.log("error", format!("{label} failed after {rows} rows ({batches}): {e}"), Some(&sql), Some(rows as i64), Some(duration_ms as i64))
                    }
                }
            }
        }
    }
}

/// Steps a resume of `job` runs: those whose last outcome was not a
/// success (failed, skipped, stopped, never run) and everything downstream
/// of them, plus any upstream step whose output is no longer available (the
/// app restarted or freed it), so the resumed steps have what they read.
/// Empty when every step succeeded.
pub fn resume_steps(ctx: &JobCtx, job: &Job) -> Vec<String> {
    let ok = |n: &JobNode| n.last_run.as_ref().is_some_and(|r| r.status == "success");
    let has_output = |n: &JobNode| {
        let tab = node_tab(&job.id, &n.id);
        ctx.engine.outputs().resolve(&n.name).is_some_and(|o| o.tab_id == tab && o.state != databrain_query_engine::OutputState::Evicted)
    };
    let mut todo: HashSet<String> = job.nodes.iter().filter(|n| !ok(n)).map(|n| n.id.clone()).collect();
    if todo.is_empty() {
        return vec![];
    }
    // Downstream of a step that runs again: its input changes.
    let mut grew = true;
    while grew {
        grew = false;
        for e in &job.edges {
            if todo.contains(&e.from) && todo.insert(e.to.clone()) {
                grew = true;
            }
        }
    }
    // Upstream outputs a resumed step reads, when they are gone.
    let mut grew = true;
    while grew {
        grew = false;
        for e in &job.edges {
            if todo.contains(&e.to) && !todo.contains(&e.from) && job.nodes.iter().find(|n| n.id == e.from).is_some_and(|n| !has_output(n)) {
                todo.insert(e.from.clone());
                grew = true;
            }
        }
    }
    job.nodes.iter().filter(|n| todo.contains(&n.id)).map(|n| n.id.clone()).collect()
}

/// Run again the steps of `job_id` that did not succeed (see [`resume_steps`]),
/// e.g. after the user fixed what made them fail.
pub fn resume(ctx: &JobCtx, job_id: &str) -> Result<JobRun> {
    let job = ctx.workspace.get_job(job_id)?;
    if ctx.workspace.job_runs(job_id, 1)?.is_empty() {
        return Err(EngineError::new("invalid", "Nothing to resume: the job has not run yet"));
    }
    let steps = resume_steps(ctx, &job);
    if steps.is_empty() {
        return Err(EngineError::new("invalid", "Nothing to resume: every step succeeded last time"));
    }
    // With the values the resumed run had.
    let last = ctx.workspace.job_runs(job_id, 1)?.into_iter().next().map(|r| r.params).unwrap_or_default();
    start_with(ctx, job_id, "resume", Some(steps), (!last.is_empty()).then_some(RunParams { set: None, values: last }))
}

/// Start a run in the background. `only`: run just these steps (their
/// upstream outputs must already exist); `None` runs the whole job.
pub fn start(ctx: &JobCtx, job_id: &str, trigger: &str, only: Option<Vec<String>>) -> Result<JobRun> {
    start_with(ctx, job_id, trigger, only, None)
}

/// [`start`] with `:name` values for this run (see [`effective_params`]).
pub fn start_with(ctx: &JobCtx, job_id: &str, trigger: &str, only: Option<Vec<String>>, over: Option<RunParams>) -> Result<JobRun> {
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
    let owner = ctx.jobs.owner.clone();
    // One process runs a job at a time (the app, or the background runner).
    match ctx.workspace.claim_job(job_id, &owner, now_ms() + LEASE_MS) {
        Ok(true) => {}
        Ok(false) => {
            ctx.jobs.running.lock().remove(job_id);
            let by = match ctx.workspace.job_lease(job_id).ok().flatten() {
                Some((o, _)) if o.starts_with("cli:") => "by the background runner",
                _ => "in another DataBrain window",
            };
            return Err(EngineError::new("busy", format!("\"{}\" is already running {by}", job.name)));
        }
        Err(e) => {
            ctx.jobs.running.lock().remove(job_id);
            return Err(e.into());
        }
    }
    let run = match ctx.workspace.start_job_run(job_id, trigger) {
        Ok(r) => r,
        Err(e) => {
            ctx.jobs.running.lock().remove(job_id);
            let _ = ctx.workspace.release_job(job_id, &owner);
            return Err(e.into());
        }
    };
    let _ = ctx.workspace.touch_job_run(run.id, &owner);
    // A missing set doesn't stop the start: the run's steps fail with why,
    // so a scheduled run reports it like any failure.
    let params = effective_params(&ctx.workspace, &job, over.as_ref());
    let mut run = run;
    if let Ok((p, _)) = &params {
        let _ = ctx.workspace.set_job_run_params(run.id, p);
        run.params = p.clone();
    }
    tracing::info!(job = job_id, run = run.id, trigger, steps = only.as_ref().map_or(job.nodes.len(), Vec::len), "job run started");
    emit(ctx, &run);
    let c = ctx.clone();
    let r = run.clone();
    tokio::spawn(async move {
        let id = r.job_id.clone();
        let run_id = r.id;
        // Keep the lease and tell other processes the run is alive.
        let beat = {
            let (ws, id, owner, stop) = (c.workspace.clone(), id.clone(), c.jobs.owner.clone(), cancel.clone());
            tokio::spawn(async move {
                let mut renewed = Instant::now();
                loop {
                    tokio::time::sleep(LEASE_RENEW).await;
                    match renew_lease(&ws, &id, &owner).await {
                        Some(true) => renewed = Instant::now(),
                        // Lost (another runner took it after the lease ran out, e.g.
                        // while the computer slept): stop, so it doesn't run twice.
                        Some(false) => {
                            tracing::warn!(job = %id, run = run_id, "job lease lost to another runner: stopping this run");
                            stop.cancel();
                            break;
                        }
                        // The workspace keeps failing: keep running while the lease
                        // still holds, stop before another runner could take it.
                        None if renewed.elapsed() + LEASE_RENEW >= Duration::from_millis(LEASE_MS as u64) => {
                            tracing::warn!(job = %id, run = run_id, "job lease could not be renewed before it runs out: stopping this run");
                            stop.cancel();
                            break;
                        }
                        None => {}
                    }
                    let _ = ws.touch_job_run(run_id, &owner);
                }
            })
        };
        execute(&c, job, r, only, cancel, params).await;
        beat.abort();
        let _ = c.workspace.release_job(&id, &c.jobs.owner);
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
            attempt: None,
            attempts: None,
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

type Params = std::result::Result<(BTreeMap<String, ParamValue>, Option<String>), String>;

async fn execute(ctx: &JobCtx, job: Job, run: JobRun, only: Option<Vec<String>>, cancel: CancellationToken, params: Params) {
    let selected: HashSet<String> = match &only {
        Some(ids) => ids.iter().cloned().collect(),
        None => job.nodes.iter().map(|n| n.id.clone()).collect(),
    };
    let mut tracer = Tracer::new(ctx, run.clone());
    let params_line = match &params {
        Ok((p, _)) if p.is_empty() => None,
        Ok((p, set)) => Some((
            "info",
            format!(
                "Parameters{}: {}",
                set.as_ref().map(|s| format!(" (set \"{s}\")")).unwrap_or_default(),
                p.iter().map(|(k, v)| format!(":{k} = {}{}", v.value, if v.raw { " (SQL)" } else { "" })).collect::<Vec<_>>().join(", ")
            ),
        )),
        Err(e) => Some(("error", e.clone())),
    };
    tracer.params = Arc::new(params.map(|(p, _)| p.into_iter().collect()));
    let trigger = match run.trigger.as_str() {
        "schedule" => "on its schedule",
        "ai" => "by the AI assistant (approved)",
        "background" => "on its schedule, by the background runner (DataBrain closed)",
        "cli" => "from the command line (databrain-jobs)",
        "resume" => "by hand, resuming the steps that did not succeed",
        _ => "by hand",
    };
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
    if let Some((level, line)) = params_line {
        tracer.log(level, line, None, None, None);
    }
    let upstream = |id: &str| -> Vec<String> { job.edges.iter().filter(|e| e.to == id).map(|e| e.from.clone()).collect() };
    // Status per selected step: pending → running → success/error/skipped/cancelled.
    let mut status: HashMap<String, &'static str> = selected.iter().map(|id| (id.clone(), "pending")).collect();
    let mut set = tokio::task::JoinSet::new();
    let started_run = Instant::now();
    // Steps at once: in the run, and per connection (`slot_key`).
    let (max_parallel, max_per_connection) = (job.limits.max_parallel(), job.limits.max_per_connection());
    let duck_id = ctx.engine.results_connection().ok().map(|c| c.id);
    let slot_key = |n: &JobNode| -> Option<String> {
        let c = match n.kind {
            JobNodeKind::Query => n.connection_id.as_ref(),
            JobNodeKind::Load => n.target_connection_id.as_ref(),
            // DuckDB writes the file.
            JobNodeKind::Export => None,
        };
        // The Results DuckDB is in this process: only the run's limit applies.
        c.filter(|c| !c.is_empty() && Some(*c) != duck_id.as_ref()).cloned()
    };
    let mut running = 0usize;
    let mut per_connection: HashMap<String, usize> = HashMap::new();
    let mut waiting: HashSet<String> = HashSet::new();
    // The run's time limit, and when stopped steps are left behind.
    let run_limit = job.limits.run_timeout();
    let far = tokio::time::Instant::now() + Duration::from_secs(365 * 86_400);
    let deadline = run_limit.map(|d| tokio::time::Instant::now() + d);
    let mut timed_out = false;
    let mut left_by: Option<tokio::time::Instant> = None;
    // Steps left behind were aborted: the loop only collects what ended.
    let mut abandoned = false;
    let timed_out_text = |d: Duration| format!("the run timed out after {} (the job's time limit)", duration_text(d));
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
                let ready = ups.iter().all(|u| status.get(u) == Some(&"success"));
                let key = slot_key(n);
                let busy_connection = key.as_ref().is_some_and(|k| per_connection.get(k).copied().unwrap_or(0) >= max_per_connection);
                let slot = running < max_parallel && !busy_connection;
                // The lease is checked (and renewed) before each step: after the
                // computer slept past it, another runner may have taken the job.
                // (Only "another runner holds it" stops here; a workspace error
                // is left to the heartbeat, which stops before the lease could expire.)
                if !cancel.is_cancelled() && ready && slot && renew_lease(&ctx.workspace, &job.id, &ctx.jobs.owner).await == Some(false) {
                    tracer.log("error", LEASE_LOST, None, None, None);
                    cancel.cancel();
                }
                // A step waiting for a slot that the user stopped on its own.
                let stopped_waiting = waiting.contains(&n.id)
                    && ctx.jobs.steps.lock().get(&(job.id.clone(), n.id.clone())).is_some_and(|t| t.is_cancelled())
                    && !cancel.is_cancelled();
                // Stopped: what hasn't run is cancelled, not skipped as if a step failed.
                let stopped = cancel.is_cancelled() || stopped_waiting || ups.iter().any(|u| status.get(u) == Some(&"cancelled"));
                if stopped {
                    status.insert(n.id.clone(), "cancelled");
                    ctx.jobs.steps.lock().remove(&(job.id.clone(), n.id.clone()));
                    let why = if timed_out {
                        format!("Not started: {}", timed_out_text(run_limit.unwrap_or_default()))
                    } else if stopped_waiting {
                        "Not started: stopped by the user while it waited".into()
                    } else {
                        "Not started: the run was stopped".into()
                    };
                    t.log("warning", why, None, None, None);
                    t.with_run(|run| {
                        run.nodes.retain(|r| r.node_id != n.id);
                        run.nodes.push(record(n, "cancelled", None, None, None));
                    });
                    changed = true;
                } else if let Some(failed) = ups.iter().find(|u| matches!(status.get(*u), Some(&"error" | &"skipped"))) {
                    status.insert(n.id.clone(), "skipped");
                    let name = job.nodes.iter().find(|x| &x.id == failed).map(|x| x.name.clone()).unwrap_or_default();
                    t.log("warning", format!("Skipped: upstream step {name} did not succeed"), None, None, None);
                    t.with_run(|run| {
                        run.nodes.retain(|r| r.node_id != n.id);
                        run.nodes.push(record(n, "skipped", None, None, Some("an upstream step failed".into())));
                    });
                    changed = true;
                } else if ready && !slot {
                    // Said once; it starts when a running step ends. Stop works on it
                    // meanwhile (its token, also used once it runs).
                    if waiting.insert(n.id.clone()) {
                        ctx.jobs.steps.lock().entry((job.id.clone(), n.id.clone())).or_insert_with(|| cancel.child_token());
                        let why = if busy_connection {
                            let conn = key.as_ref().and_then(|k| ctx.workspace.get_connection(k).ok()).map(|p| p.name).unwrap_or_default();
                            format!("{max_per_connection} steps are running on {conn} (the job's limit per connection)")
                        } else {
                            format!("{max_parallel} steps are running (the job's limit)")
                        };
                        t.log("info", format!("Waiting: {why}"), None, None, None);
                        t.with_run(|run| {
                            run.nodes.retain(|r| r.node_id != n.id);
                            run.nodes.push(record(n, "waiting", None, None, Some(why)));
                        });
                    }
                } else if ready {
                    status.insert(n.id.clone(), "running");
                    running += 1;
                    if let Some(k) = &key {
                        *per_connection.entry(k.clone()).or_default() += 1;
                    }
                    t.log("info", format!("Started ({})", action_text(n)), None, None, None);
                    t.with_run(|run| {
                        run.nodes.retain(|r| r.node_id != n.id);
                        run.nodes.push(record(n, "running", None, None, None));
                    });
                    let step_cancel = ctx.jobs.steps.lock().entry((job.id.clone(), n.id.clone())).or_insert_with(|| cancel.child_token()).clone();
                    let (c, node, job_id, job_name, cancel) = (ctx.clone(), n.clone(), job.id.clone(), job.name.clone(), step_cancel);
                    let ups_names: Vec<String> = upstream(&n.id).iter().filter_map(|u| job.nodes.iter().find(|x| &x.id == u).map(|x| x.name.clone())).collect();
                    let tries = (n.limits.timeout(&job.limits), n.limits.retries(&job.limits));
                    set.spawn(async move {
                        let started = Instant::now();
                        // A panic anywhere in the step (also outside the SQL it runs)
                        // fails it, so the step's slot is given back and the run goes on.
                        let out = std::panic::AssertUnwindSafe(run_step(&c, &job_id, &job_name, &node, &ups_names, &cancel, &t, tries))
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| Err(EngineError::new("internal", "the step stopped unexpectedly")));
                        (node, started, out, t)
                    });
                    changed = true;
                }
            }
        }
        tracer.save();
        if cancel.is_cancelled() && left_by.is_none() {
            left_by = Some(tokio::time::Instant::now() + STOP_GRACE);
        }
        // A waiting step stopped on its own wakes the loop (it isn't in `set`).
        let waiting_tokens: Vec<CancellationToken> = {
            let steps = ctx.jobs.steps.lock();
            waiting.iter().filter(|id| status.get(*id) == Some(&"pending")).filter_map(|id| steps.get(&(job.id.clone(), id.clone())).cloned()).collect()
        };
        let waiting_stopped = async {
            if waiting_tokens.is_empty() {
                std::future::pending::<()>().await
            } else {
                futures::future::select_all(waiting_tokens.iter().map(|t| Box::pin(t.cancelled()))).await;
            }
        };
        let done = tokio::select! {
            // Steps that ended first: a run whose last step ends as its time
            // limit comes isn't marked timed out.
            biased;
            done = set.join_next() => done,
            _ = waiting_stopped, if !cancel.is_cancelled() => continue,
            // Stop: steps waiting on upstream steps (no token of their own)
            // show as not started now, not when a running step returns.
            _ = cancel.cancelled(), if !cancel.is_cancelled() => continue,
            _ = tokio::time::sleep_until(deadline.unwrap_or(far)), if deadline.is_some() && !timed_out && !cancel.is_cancelled() => {
                timed_out = true;
                tracer.log("error", format!("Stopping: {}", timed_out_text(run_limit.unwrap_or_default())), None, None, None);
                cancel.cancel();
                continue;
            }
            // Steps that don't end once stopped are left behind (their
            // sessions are closed below), so the run ends and lets go of the job.
            _ = tokio::time::sleep_until(left_by.unwrap_or(far)), if left_by.is_some() && !abandoned && !set.is_empty() => {
                tracer.log("warning", format!("{} steps did not stop within {}: left behind", set.len(), duration_text(STOP_GRACE)), None, None, None);
                set.abort_all();
                // The loop goes on until the set is empty: a step that
                // finished meanwhile (an export moved into place) is
                // recorded as it ended; the aborted ones are left behind.
                abandoned = true;
                continue;
            }
        };
        let Some(done) = done else { break };
        let Ok((node, started, out, t)) = done else { continue };
        running = running.saturating_sub(1);
        if let Some(k) = slot_key(&node)
            && let Some(c) = per_connection.get_mut(&k)
        {
            *c = c.saturating_sub(1);
        }
        let step_token = ctx.jobs.steps.lock().remove(&(job.id.clone(), node.id.clone()));
        let step_stopped = step_token.is_some_and(|t| t.is_cancelled());
        let took = started.elapsed().as_millis() as i64;
        let rec = match out {
            Ok(out) => {
                status.insert(node.id.clone(), "success");
                t.log(
                    "success",
                    format!("Succeeded: {} rows{}", out.rows, out.file.as_ref().map(|f| format!(", file {f}")).unwrap_or_default()),
                    None,
                    Some(out.rows),
                    Some(took),
                );
                let mut r = record(&node, "success", Some(started), Some(out.rows), None);
                r.summary.file = out.file;
                r.summary.notices = out.notices;
                r
            }
            Err(e) if cancel.is_cancelled() || step_stopped || e.kind == "cancelled" => {
                status.insert(node.id.clone(), "cancelled");
                let why = if timed_out {
                    format!("Stopped: {}", timed_out_text(run_limit.unwrap_or_default()))
                } else if step_stopped && !cancel.is_cancelled() {
                    "Stopped by the user".into()
                } else {
                    "Cancelled".into()
                };
                t.log("warning", why, None, None, Some(took));
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
                let (attempt, attempts) = (r.summary.attempt, r.summary.attempts);
                *r = rec;
                (r.summary.attempt, r.summary.attempts) = (attempt, attempts);
            }
        });
    }
    // Steps left behind: stopped, as far as the run is concerned.
    for n in &job.nodes {
        if status.get(&n.id) == Some(&"running") {
            status.insert(n.id.clone(), "cancelled");
            ctx.jobs.steps.lock().remove(&(job.id.clone(), n.id.clone()));
            tracer.with_run(|run| {
                if let Some(r) = run.nodes.iter_mut().find(|r| r.node_id == n.id) {
                    *r = record(n, "cancelled", None, None, Some(format!("did not stop within {}: left behind", duration_text(STOP_GRACE))));
                }
            });
        }
    }
    for n in &job.nodes {
        ctx.engine.release_sessions(&node_tab(&job.id, &n.id));
        ctx.engine.release_sessions(&step_load(&node_tab(&job.id, &n.id)));
        ctx.engine.release_sessions(&step_export(&node_tab(&job.id, &n.id)));
    }
    let any = |s: &str| status.values().any(|v| *v == s);
    for (id, st) in &status {
        if *st == "error" {
            tracing::warn!(job = %job.id, step = %id, "job step failed");
        }
    }
    // A run stopped by its time limit failed.
    let final_status = if any("error") || any("skipped") || timed_out {
        "error"
    } else if any("cancelled") || cancel.is_cancelled() {
        "cancelled"
    } else {
        "success"
    };
    let nodes = tracer.with_run(|run| {
        run.status = final_status.into();
        run.error = match run_limit.filter(|_| timed_out) {
            Some(d) => Some(format!("Stopped: {}", timed_out_text(d))),
            None => run.nodes.iter().find(|r| r.summary.status == "error").map(|r| format!("{}: {}", r.name, r.summary.error.clone().unwrap_or_default())),
        };
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
    tracing::info!(job = %job.id, run = run.id, trigger = %run.trigger, status = final_status, steps = nodes.len(), ms = started_run.elapsed().as_millis() as u64, "job run finished");
}

/// Run one step: each try within its time limit (`tries.0`), tried again
/// up to `tries.1` times after a connection error when that can't write
/// twice ([`retry_refused`]), waiting longer each time.
#[allow(clippy::too_many_arguments)]
async fn run_step(
    ctx: &JobCtx,
    job_id: &str,
    job_name: &str,
    node: &JobNode,
    upstreams: &[String],
    cancel: &CancellationToken,
    t: &Tracer,
    tries: (Option<Duration>, u32),
) -> Result<NodeOut> {
    let (limit, retries) = tries;
    let of = retries + 1;
    let mut n = 1;
    loop {
        if of > 1 {
            t.attempt(n, of);
        }
        let e = match one_try(ctx, job_id, job_name, node, upstreams, cancel, t, limit).await {
            Ok(out) => return Ok(out),
            Err(e) => e,
        };
        if n >= of || cancel.is_cancelled() || !retryable(&e) {
            return Err(e);
        }
        if let Some(why) = retry_refused(ctx, node, t) {
            t.log("warning", format!("Not tried again: {why}"), None, None, None);
            return Err(e);
        }
        let wait = databrain_workspace::retry_delay(ctx.jobs.retry_base(), n);
        t.log("warning", format!("Try {n} of {of} failed ({}): trying again in {}", e.message, duration_text(wait)), None, None, None);
        // Saved now: the wait may be long (up to 30 min), and the app shows
        // the background runner's runs from the workspace.
        t.save();
        // A new session: the old one may be broken.
        let tab = node_tab(job_id, &node.id);
        for key in [tab.clone(), step_load(&tab), step_export(&tab)] {
            ctx.engine.release_sessions(&key);
        }
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = cancel.cancelled() => return Err(e),
        }
        n += 1;
    }
}

/// One try of a step, stopped when it takes longer than `limit`. Once it
/// is stopped (its time limit, the user stopping it or the run), it gets
/// [`STOP_GRACE`] to end; then it is left behind (dropped, its sessions
/// are closed when the run ends), so a statement that ignores Stop can't
/// keep the run and its lease going.
#[allow(clippy::too_many_arguments)]
async fn one_try(
    ctx: &JobCtx,
    job_id: &str,
    job_name: &str,
    node: &JobNode,
    upstreams: &[String],
    cancel: &CancellationToken,
    t: &Tracer,
    limit: Option<Duration>,
) -> Result<NodeOut> {
    // A try of an earlier run left behind (it didn't stop when asked) may
    // still hold the step's sessions: say so, not "a query is already running".
    let tab = node_tab(job_id, &node.id);
    if [tab.clone(), step_load(&tab), step_export(&tab)].iter().any(|k| ctx.engine.in_use(k)) {
        return Err(EngineError::new("busy", STILL_RUNNING));
    }
    let stop = cancel.child_token();
    // A panic fails this step only (the run must still finish).
    let work = std::panic::AssertUnwindSafe(run_node(ctx, job_id, job_name, node, upstreams, &stop, t)).catch_unwind();
    let panicked = |_| Err(EngineError::new("internal", "the step stopped unexpectedly"));
    tokio::pin!(work);
    let over_limit = async {
        match limit {
            Some(l) => tokio::time::sleep(l).await,
            None => std::future::pending().await,
        }
    };
    let left_behind = || EngineError::new("cancelled", format!("did not stop within {}: left behind", duration_text(STOP_GRACE)));
    tokio::select! {
        biased;
        out = &mut work => out.unwrap_or_else(panicked),
        _ = cancel.cancelled() => match tokio::time::timeout(STOP_GRACE, &mut work).await {
            Ok(out) => out.unwrap_or_else(panicked),
            Err(_) => {
                t.log("warning", format!("The step did not stop within {}: left behind", duration_text(STOP_GRACE)), None, None, None);
                Err(left_behind())
            }
        },
        _ = over_limit => {
            let limit = limit.unwrap_or_default();
            t.log("error", format!("Time limit reached ({}): stopping the step", duration_text(limit)), None, None, None);
            stop.cancel();
            // What finished anyway is kept (a load may have committed).
            match tokio::time::timeout(STOP_GRACE, &mut work).await {
                Ok(Ok(Ok(out))) => Ok(out),
                _ => Err(EngineError::new("timeout", format!("Timed out after {} (the step's time limit)", duration_text(limit)))),
            }
        }
    }
}

/// Errors worth trying again: the connection (couldn't connect, lost it,
/// timed out reaching the server). Not the step's own time limit, a
/// refused sign-in, or an error in the SQL.
fn retryable(e: &EngineError) -> bool {
    e.kind == "connection"
}

/// Why a step that failed must not run again (`None`: it may), so a retry
/// can't write twice: only SQL that reads is run again (every statement a
/// read, no function that isn't a known built-in), and a load only when it
/// wrote nothing or ran in one transaction that was rolled back.
fn retry_refused(ctx: &JobCtx, node: &JobNode, t: &Tracer) -> Option<String> {
    let Ok(params) = t.params() else { return Some("the run's parameter values are missing".into()) };
    let reads = |sql: &str, connection: Option<&str>| {
        let kind = match connection.filter(|c| !c.is_empty()) {
            Some(c) => ctx.workspace.get_connection(c).ok().map(|p| p.config.kind),
            None => Some(databrain_connector_core::ConnectorKind::Duckdb),
        };
        kind.is_some_and(|k| reads_only(sql, k, &params))
    };
    let may_write = "its SQL may write (only steps that read are tried again)";
    match node.kind {
        JobNodeKind::Query => (!reads(&node.sql, node.connection_id.as_deref())).then(|| may_write.to_string()),
        JobNodeKind::Load | JobNodeKind::Export if !node.sql.trim().is_empty() && !reads(&node.sql, None) => Some(may_write.into()),
        JobNodeKind::Export => None,
        JobNodeKind::Load => load_retry_refused(*t.load.lock()).map(str::to_string),
    }
}

/// Why a load step's try must not run again, by how far its load got:
/// only when it never started or said it kept nothing.
fn load_retry_refused(load: LoadState) -> Option<&'static str> {
    match load {
        LoadState::NotStarted | LoadState::KeptNothing => None,
        LoadState::Started => Some("the load didn't say whether it kept any of its writes"),
        LoadState::MayHaveKept => Some("the load may have kept some of its writes (it didn't run in one transaction that was rolled back)"),
    }
}

/// Whether every statement of `sql` (with the run's `:name` values) only
/// reads: classified as a read, calling no function that isn't a known
/// built-in. Unsure is not a read.
pub fn reads_only(sql: &str, kind: databrain_connector_core::ConnectorKind, params: &HashMap<String, ParamValue>) -> bool {
    use databrain_connector_core::sql::{Checked, split_statements, unknown_functions};
    let spans = split_statements(sql, kind);
    !spans.is_empty()
        && spans.iter().all(|s| match Checked::new(s.sql.clone(), kind).bind(kind, params) {
            Ok(c) => c.kind().is_read() && unknown_functions(c.sql(), kind).is_empty(),
            Err(_) => false,
        })
}

/// SQL a load step runs when it has none: its upstream's output.
pub fn default_load_sql(upstreams: &[String]) -> Option<String> {
    upstreams.first().map(|u| format!("select * from results.{}", databrain_query_engine::outputs::q_min(u)))
}

/// Folder an export step writes to: its own, else the Downloads folder.
pub fn export_folder(node: &JobNode) -> Result<std::path::PathBuf> {
    let dir = match node.export_folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => std::path::PathBuf::from(f),
        None => dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .ok_or_else(|| EngineError::new("invalid", "Choose the folder to save the file in"))?,
    };
    if !dir.is_absolute() {
        return Err(EngineError::new("invalid", format!("The folder must be a full path: {}", dir.display())));
    }
    if !dir.is_dir() {
        return Err(EngineError::new("invalid", format!("Folder not found: {}", dir.display())));
    }
    Ok(dir)
}

/// Files the export steps of `job_id` wrote in its kept runs.
fn files_written_by(ws: &Workspace, job_id: &str) -> HashSet<String> {
    let runs = ws.job_runs(job_id, databrain_workspace::jobs::RUNS_KEPT).unwrap_or_default();
    runs.into_iter().flat_map(|r| r.nodes.into_iter().filter_map(|n| n.summary.file)).collect()
}

/// Where an export of `name` into `folder` goes, and whether it may replace
/// the file there: only a file an earlier run of the same job wrote (a fixed
/// name like `report.csv` is updated each run). Any other file, or a link,
/// is kept and the export is saved as `name (2)`, `name (3)`…
pub fn export_target(folder: &std::path::Path, name: &str, written: &HashSet<String>) -> Result<(std::path::PathBuf, bool)> {
    let p = std::path::Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p.extension().and_then(|e| e.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    let candidates = std::iter::once(folder.join(name)).chain((2..1000).map(|i| folder.join(format!("{stem} ({i}){ext}"))));
    for c in candidates {
        match std::fs::symlink_metadata(&c) {
            Err(_) => return Ok((c, false)),
            Ok(m) if m.is_file() && written.contains(&*c.to_string_lossy()) => return Ok((c, true)),
            Ok(_) => {}
        }
    }
    Err(EngineError::new("invalid", format!("{name} and its numbered copies already exist in {}", folder.display())))
}

/// Move the finished export `tmp` to `dest`. Without `replace` a file that
/// appeared at `dest` meanwhile is kept (the export fails instead).
/// `Ok(Some(notice))`: the file is in place, but the temporary file
/// couldn't be removed (the export still counts: `dest` is written).
fn finish_export(tmp: &std::path::Path, dest: &std::path::Path, replace: bool) -> std::io::Result<Option<String>> {
    if replace {
        return std::fs::rename(tmp, dest).map(|_| None);
    }
    match std::fs::hard_link(tmp, dest) {
        Ok(()) => Ok(std::fs::remove_file(tmp).err().map(|e| format!("the temporary file {} could not be removed ({e}): delete it by hand", tmp.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        // Folders without hard links (some network and FAT drives).
        Err(_) if std::fs::symlink_metadata(dest).is_err() => std::fs::rename(tmp, dest).map(|_| None),
        Err(e) => Err(e),
    }
}

/// An export's temporary file, removed when the step ends without moving
/// it into place, also when the step is dropped (its time limit, or left
/// behind after Stop): [`PartFile::keep`] once it is in place.
struct PartFile(Option<std::path::PathBuf>);

impl PartFile {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The temporary file name of an export of step `node_id` of `job_id`:
/// named after the step, so files a step left behind (DuckDB still writing
/// when it was dropped, the app quit) are found and removed by its next run.
fn part_prefix(job_id: &str, node_id: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect::<String>();
    format!(".databrain-{}-{}.", clean(job_id), clean(node_id))
}

/// Remove the temporary files earlier runs of the step left in `folder`.
fn sweep_parts(folder: &std::path::Path, prefix: &str) {
    let Ok(dir) = std::fs::read_dir(folder) else { return };
    for e in dir.flatten() {
        let name = e.file_name();
        let Some(n) = name.to_str() else { continue };
        if n.starts_with(prefix) && n.ends_with(".part") {
            let _ = std::fs::remove_file(e.path());
        }
    }
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
#[allow(clippy::too_many_arguments)]
async fn run_sql(
    ctx: &JobCtx,
    connection_id: String,
    tab: String,
    sql: String,
    output_name: Option<String>,
    origin: Origin,
    cancel: &CancellationToken,
    t: &Tracer,
) -> Result<Vec<databrain_query_engine::StatementOutcomeView>> {
    let conn = ctx.workspace.get_connection(&connection_id).map(|p| p.name).unwrap_or_else(|_| connection_id.clone());
    // The run's `:name` values (job parameters); a missing one fails the step.
    let params = t.params()?;
    t.log("info", format!("Running SQL on {conn}"), None, None, None);
    let outcomes = ctx
        .engine
        .run_and_wait(
            &ctx.hub,
            RunRequest {
                connection_id,
                tab_id: tab,
                sql,
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin,
                session_key: None,
                output_name,
                params,
                output_masks: None,
            },
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
            (None, None, rows) => t.log(
                "info",
                format!("{which}: done{}", rows.map(|r| format!(", {r} rows affected")).unwrap_or_default()),
                Some(&o.sql),
                rows.map(|r| r as i64),
                Some(o.duration_ms as i64),
            ),
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
        JobNodeKind::Query => (
            match &node.connection_id {
                Some(c) if !c.is_empty() => c.clone(),
                _ => duck()?,
            },
            node.sql.clone(),
        ),
        JobNodeKind::Load | JobNodeKind::Export => {
            let sql = if node.sql.trim().is_empty() {
                default_load_sql(upstreams)
                    .ok_or_else(|| EngineError::new("invalid", "Write the SQL that selects the rows to load, or connect an upstream step"))?
            } else {
                node.sql.clone()
            };
            (duck()?, sql)
        }
    };
    if sql.trim().is_empty() {
        return Err(EngineError::new("invalid", "This step has no SQL"));
    }
    // Rows the assistant's SQL selected (an output it made: a step whose
    // SQL it wrote, a query of its own) stay the assistant's: a step that
    // reads them, by its SQL or by default from its upstream, runs under the
    // AI rules too, so a load or export the user wrote can't take rows the
    // assistant chose where the assistant may not write. Every DuckDB
    // connection reads `results.*` (Results, a DuckDB file, MotherDuck), so
    // a query step on any of them is followed; the step's own output then is
    // the AI's as well, so this follows the rows down the job.
    let on_results = node.kind != JobNodeKind::Query || reads_outputs(ctx, &connection_id)?;
    let ai_rows: Vec<String> = if on_results {
        ctx.engine.outputs().referenced(&sql).into_iter().filter(|o| o.origin == Origin::Ai).map(|o| o.name.unwrap_or(o.handle)).collect()
    } else {
        vec![]
    };
    let origin = if ai_rows.is_empty() { t.step_origin(node) } else { Origin::Ai };
    if origin == Origin::Ai && t.step_origin(node) != Origin::Ai {
        t.log(
            "info",
            format!(
                "Runs under the AI rules: it reads rows the assistant's SQL selected ({})",
                ai_rows.iter().map(|n| format!("results.{n}")).collect::<Vec<_>>().join(", ")
            ),
            None,
            None,
            None,
        );
    }
    // SQL the assistant wrote (and every step of a run it started, and of
    // rows it chose) passes the connection's AI rules as they are now: the
    // job was approved under the rules of that day, and since then the
    // connection may have become production or read-only, or lost "AI may
    // write" or a table.
    if origin == Origin::Ai {
        ai_rules_now(ctx, node, &sql, &connection_id, t).await?;
    }
    // Checked before the query runs, so a missing folder fails fast.
    let folder = if node.kind == JobNodeKind::Export { Some(export_folder(node)?) } else { None };
    let duck_id = connection_id.clone();
    // Outputs it reads that were capped (a row limit, or the memory a
    // result may keep): working on them would pass part of the rows on as
    // if it were all (a Sync load would delete the rest). Checked before
    // the step runs, so nothing of it is written.
    let capped = |o: &databrain_query_engine::OutputInfo, what: &str| {
        EngineError::new(
            "invalid",
            format!(
                "{} holds only its first {} rows (capped by a row limit or by the memory a result may keep): nothing was {what}. Run it without a row limit, select fewer columns, or split the step.",
                o.name.as_deref().unwrap_or(&o.handle),
                o.rows
            ),
        )
    };
    // Every DuckDB connection reads `results.*` (the same rule as above); a
    // query on another database naming `results.x` reads its own table.
    if on_results && let Some(o) = ctx.engine.outputs().referenced(&sql).into_iter().find(|o| o.truncated) {
        return Err(capped(&o, "run"));
    }
    let outcomes = run_sql(ctx, connection_id, tab.clone(), sql, Some(node.name.clone()), origin, cancel, t).await?;
    let last = outcomes.last();
    let rows = last.and_then(|o| o.result.as_ref().map(|r| r.total_rows as i64).or(o.rows_affected.map(|r| r as i64))).unwrap_or(0);
    if node.kind == JobNodeKind::Query {
        // A capped result of its own fails the steps that read it (above);
        // a step nothing reads still says it holds only part of the rows.
        let own = ctx.engine.outputs().for_tab(&tab).into_iter().rev().find(|o| o.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&node.name)));
        if let Some(o) = own.as_ref().filter(|o| o.truncated) {
            t.log(
                "warning",
                format!(
                    "{} holds only its first {} rows (capped by a row limit or by the memory a result may keep): a step that reads it fails",
                    o.name.as_deref().unwrap_or(&o.handle),
                    o.rows
                ),
                None,
                None,
                None,
            );
        }
        return Ok(NodeOut { rows, file: None, notices: vec![] });
    }
    // This step's result (a statement that returns rows ends with one).
    let output = ctx
        .engine
        .outputs()
        .for_tab(&tab)
        .into_iter()
        .rev()
        .find(|o| o.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&node.name)))
        .ok_or_else(|| EngineError::new("invalid", "The SQL returned no rows (it must end with a SELECT)"))?;
    if output.truncated {
        return Err(capped(&output, if node.kind == JobNodeKind::Export { "exported" } else { "loaded" }));
    }
    if let Some(folder) = folder {
        let name = databrain_workspace::jobs::export_file_name(node.export_file.as_deref(), &node.name, job_name, node.export_format, now_ms());
        let written = files_written_by(&ctx.workspace, job_id);
        let (path, replace) = export_target(&folder, &name, &written)?;
        let mut notices = vec![];
        if path.file_name().and_then(|n| n.to_str()) != Some(name.as_str()) {
            notices.push(format!("{name} already exists and wasn't written by this job: saved as {}", path.display()));
        }
        // Written next to the file, then moved into place: a failed or
        // cancelled export leaves the old file whole. Its earlier runs'
        // leftovers go first.
        let prefix = part_prefix(job_id, &node.id);
        sweep_parts(&folder, &prefix);
        let tmp = folder.join(format!("{prefix}{}.part", uuid::Uuid::new_v4().simple()));
        let part = PartFile(Some(tmp.clone()));
        // By handle: exactly this run's rows, even if another step has the same name.
        let select = format!("select * from results.{}", output.handle);
        t.log("info", format!("Writing {} rows to {}", output.rows, path.display()), None, None, None);
        run_sql(ctx, duck_id, step_export(&tab), copy_sql(&select, &tmp, node.export_format), None, Origin::Job, cancel, t).await?;
        let left = finish_export(&tmp, &path, replace).map_err(|e| EngineError::new("io", format!("cannot save {}: {e}", path.display())))?;
        part.keep();
        if let Some(n) = left {
            t.log("warning", n.clone(), None, None, None);
            notices.push(n);
        }
        return Ok(NodeOut { rows: output.rows as i64, file: Some(path.to_string_lossy().into_owned()), notices });
    }
    let target =
        node.target_connection_id.as_deref().filter(|c| !c.is_empty()).ok_or_else(|| EngineError::new("invalid", "Choose the connection to load into"))?;
    let spec = load_spec_now(ctx, node, target, origin, t)?;
    // From here a failure may have written: a retry needs the load to say it kept nothing.
    *t.load.lock() = LoadState::Started;
    let report = ctx.engine.write_output(&output.handle, target, &step_load(&tab), &spec, cancel, t).await?;
    Ok(NodeOut { rows: report.rows as i64, file: None, notices: report.notices })
}

/// [`load_spec`] with the run's `:name` values in the before/after SQL.
fn load_spec_now(ctx: &JobCtx, node: &JobNode, target: &str, origin: Origin, t: &Tracer) -> Result<LoadSpec> {
    let mut spec = load_spec(node);
    // Under the AI rules (a run it started, the assistant's SQL or rows),
    // the user's before/after SQL runs as the assistant's too (sandboxed on
    // a DuckDB target), as the step's own SQL.
    spec.origin = origin;
    let kind = ctx.workspace.get_connection(target)?.config.kind;
    let params = t.params()?;
    for sql in [&mut spec.before_sql, &mut spec.after_sql].into_iter().flatten() {
        *sql = databrain_connector_core::sql::substitute_parameters(sql, kind, &params).map_err(|missing| {
            EngineError::new(
                "invalid",
                format!(
                    "Enter a value for {} in the job's parameters (before/after SQL)",
                    missing.iter().map(|n| format!(":{n}")).collect::<Vec<_>>().join(", ")
                ),
            )
        })?;
    }
    Ok(spec)
}

/// Whether SQL on `connection_id` can read the outputs (`results.*`): every
/// DuckDB connection loads them, Results and the user's DuckDB files alike.
fn reads_outputs(ctx: &JobCtx, connection_id: &str) -> Result<bool> {
    if connection_id == ctx.engine.results_connection()?.id {
        return Ok(true);
    }
    Ok(ctx.workspace.get_connection(connection_id).is_ok_and(|p| p.config.kind == databrain_connector_core::ConnectorKind::Duckdb))
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
        origin: sql_origin(node),
        // The job was saved to write there; its runs don't ask.
        confirmed: true,
    }
}

/// A step's SQL as it will run (`sql`, with the run's `:name` values in
/// place where they are known), checked like the assistant's own queries
/// ([`databrain_ai::jobs::review_step`]). What only the user could approve
/// was approved with the job and is not asked again. The table rules
/// complete unqualified names with the schema each connection's server
/// says is current, asked now: `databrain-jobs tick` is a new process each
/// time, and in the app what was learned before may be out of date.
async fn ai_rules_now(ctx: &JobCtx, node: &JobNode, sql: &str, connection_id: &str, t: &Tracer) -> Result<()> {
    let mut ids: Vec<&str> = vec![connection_id];
    for c in [&node.connection_id, &node.target_connection_id].into_iter().flatten() {
        if !c.is_empty() && !ids.contains(&c.as_str()) {
            ids.push(c);
        }
    }
    for id in ids {
        if let Ok(p) = ctx.workspace.get_connection(id) {
            databrain_ai::access::learn_defaults(&ctx.engine, &p).await;
        }
    }
    let params = t.params().unwrap_or_default();
    let filled = |sql: &str, connection: Option<&str>| -> String {
        let kind = connection.and_then(|c| ctx.workspace.get_connection(c).ok()).map(|p| p.config.kind);
        kind.and_then(|k| databrain_connector_core::sql::substitute_parameters(sql, k, &params).ok()).unwrap_or_else(|| sql.to_string())
    };
    let mut checked = node.clone();
    checked.sql = filled(sql, Some(connection_id));
    let target = node.target_connection_id.clone();
    for s in [&mut checked.load_before_sql, &mut checked.load_after_sql].into_iter().flatten() {
        *s = filled(s, target.as_deref());
    }
    databrain_ai::jobs::review_step(&ctx.engine, &checked).map(|_| ()).map_err(|e| {
        let why = match e {
            databrain_ai::AiError::Policy(r) => r,
            other => other.to_string(),
        };
        EngineError::new(
            "invalid",
            format!("Not run: the connection's AI settings no longer allow what the assistant wrote ({why}). See the connection's AI settings."),
        )
    })
}

/// Whose SQL a step's is outside a run (dry runs, loads): the AI's when it
/// wrote it, else the job's.
fn sql_origin(node: &JobNode) -> Origin {
    if node.ai_sql { Origin::Ai } else { Origin::Job }
}

/// Rows a dry run selects to try the load with.
pub const DRY_RUN_ROWS: usize = 1000;

/// `:name` values a dry run selects its rows with: the job's.
fn dry_run_params(ctx: &JobCtx, job: &Job) -> HashMap<String, ParamValue> {
    effective_params(&ctx.workspace, job, None).map(|(p, _)| p.into_iter().collect()).unwrap_or_default()
}

/// Check a load step of `job` (as edited, not necessarily saved) without
/// keeping any change: its SQL selects up to [`DRY_RUN_ROWS`] rows on
/// DuckDB, then the load is tried with them (see
/// [`QueryEngine::dry_run_load`]).
pub async fn dry_run(ctx: &JobCtx, job: &Job, node_id: &str) -> Result<LoadDryRun> {
    let node = job.nodes.iter().find(|n| n.id == node_id).ok_or_else(|| EngineError::new("not_found", "Step not found"))?;
    if node.kind != JobNodeKind::Load {
        return Err(EngineError::new("invalid", "Only load steps have a dry run"));
    }
    let target =
        node.target_connection_id.as_deref().filter(|c| !c.is_empty()).ok_or_else(|| EngineError::new("invalid", "Choose the connection to load into"))?;
    let ups: Vec<String> =
        job.edges.iter().filter(|e| e.to == node.id).filter_map(|e| job.nodes.iter().find(|n| n.id == e.from).map(|n| n.name.clone())).collect();
    let sql = if node.sql.trim().is_empty() { default_load_sql(&ups) } else { Some(node.sql.clone()) };
    // Not a `job:` tab: its result is not the step's output. One per dry
    // run: two at once on one session would share (and end) a transaction.
    let tab = databrain_workspace::SessionRole::DryRun(format!("{}:{}:{}", job.id, node.id, uuid::Uuid::new_v4().simple())).key();
    let cancel = CancellationToken::new();
    let (rows, rows_error) = match sql {
        None => (None, Some("Write the SQL that selects the rows to load, or connect an upstream step".to_string())),
        Some(sql) => {
            let duck = ctx.engine.results_connection()?.id;
            let req = RunRequest {
                connection_id: duck,
                tab_id: tab.clone(),
                sql,
                base_offset: 0,
                row_limit: Some(DRY_RUN_ROWS),
                confirmed: true,
                origin: sql_origin(node),
                session_key: None,
                output_name: None,
                params: dry_run_params(ctx, job),
                output_masks: None,
            };
            match ctx.engine.run_and_wait(&ctx.hub, req, Some(cancel.clone())).await {
                Err(e) => (None, Some(e.message)),
                Ok(outcomes) => match outcomes.iter().find_map(|o| o.error.as_ref()) {
                    Some(e) => {
                        (None, Some(format!("{}{}", e.message, if ups.is_empty() { "" } else { " (run the upstream steps first so their outputs exist)" })))
                    }
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
    let report = ctx.engine.dry_run_load(rows.as_ref(), rows_error, target, &step_load(&tab), &load_spec(node)).await;
    ctx.engine.close_tab(&tab);
    ctx.engine.release_sessions(&step_load(&tab));
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

/// One scheduler pass; returns the jobs started. `trigger`: `schedule`
/// (the app) or `background` (the runner while the app is closed).
pub fn tick(ctx: &JobCtx) -> Vec<String> {
    tick_as(ctx, "schedule")
}

pub fn tick_as(ctx: &JobCtx, trigger: &str) -> Vec<String> {
    // Runs whose runner stopped (another process closed or crashed).
    let _ = ctx.workspace.close_stale_job_runs();
    let now = now_ms();
    let owner = ctx.jobs.owner.clone();
    let mut started = Vec::new();
    for j in ctx.workspace.list_jobs().unwrap_or_default() {
        if !j.schedule.enabled || ctx.jobs.is_running(&j.id) || j.next_run_at.is_none_or(|t| t > now) {
            continue;
        }
        // Take the job, then look again: another process may have just run it.
        if !ctx.workspace.claim_job(&j.id, &owner, now_ms() + LEASE_MS).unwrap_or(false) {
            continue;
        }
        let fresh = ctx.workspace.get_job(&j.id).ok();
        let due = fresh.as_ref().and_then(|f| f.schedule.next_after(f.schedule_base())).is_some_and(|t| t <= now);
        if !due {
            let _ = ctx.workspace.release_job(&j.id, &owner);
            continue;
        }
        // Marked while the lease is held (a fast run could otherwise end and
        // release it before the mark, and another runner start it again)…
        let before = fresh.and_then(|f| f.last_scheduled_at);
        let _ = ctx.workspace.mark_job_scheduled(&j.id, now);
        match start(ctx, &j.id, trigger, None) {
            Ok(_) => started.push(j.id),
            // …and given back when the run didn't start: the slot isn't used up.
            Err(e) => {
                tracing::warn!(job = %j.id, error = %e.message, "scheduled job did not start; it is tried again on the next tick");
                let _ = ctx.workspace.set_job_last_scheduled(&j.id, before);
                let _ = ctx.workspace.release_job(&j.id, &owner);
            }
        }
    }
    started
}

/// Start a run and wait for it to end (command line). Returns the run as saved.
pub async fn run_and_wait(ctx: &JobCtx, job_id: &str, trigger: &str) -> Result<JobRun> {
    run_and_wait_with(ctx, job_id, trigger, None).await
}

/// [`run_and_wait`] with `:name` values for this run.
pub async fn run_and_wait_with(ctx: &JobCtx, job_id: &str, trigger: &str, params: Option<RunParams>) -> Result<JobRun> {
    let run = start_with(ctx, job_id, trigger, None, params)?;
    wait_for(ctx, &[job_id.to_string()]).await;
    Ok(ctx.workspace.job_runs(job_id, 5)?.into_iter().find(|r| r.id == run.id).unwrap_or(run))
}

/// Wait until these jobs are no longer running in this process.
pub async fn wait_for(ctx: &JobCtx, ids: &[String]) {
    while ids.iter().any(|id| ctx.jobs.is_running(id)) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::ConnectorKind::{Duckdb, Mongodb, Mysql, Postgres};

    /// A load step's try runs again only when its load never started or said
    /// it kept nothing; a load that started and didn't say (or said it may
    /// have kept writes) isn't tried again.
    #[test]
    fn load_retry_needs_the_load_to_say_it_kept_nothing() {
        assert_eq!(load_retry_refused(LoadState::NotStarted), None);
        assert_eq!(load_retry_refused(LoadState::KeptNothing), None);
        assert!(load_retry_refused(LoadState::Started).is_some_and(|w| w.contains("didn't say")));
        assert!(load_retry_refused(LoadState::MayHaveKept).is_some_and(|w| w.contains("may have kept")));
    }

    /// An export's temporary file goes when the step ends without moving it
    /// into place (dropped by a time limit or left behind too), stays once
    /// kept; the next run of the step removes what earlier ones left, and
    /// only theirs.
    #[test]
    fn export_temp_files_never_stay_behind() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = part_prefix("job/1", "n:1");
        assert_eq!(prefix, ".databrain-job_1-n_1.");
        let tmp = dir.path().join(format!("{prefix}abc.part"));
        std::fs::write(&tmp, "x").unwrap();
        drop(PartFile(Some(tmp.clone())));
        assert!(!tmp.exists(), "dropped: removed");
        std::fs::write(&tmp, "x").unwrap();
        PartFile(Some(tmp.clone())).keep();
        assert!(tmp.exists(), "kept once in place");
        let other_step = dir.path().join(format!("{}abc.part", part_prefix("job/1", "n:2")));
        let users = dir.path().join(".report.csv.part");
        std::fs::write(&other_step, "x").unwrap();
        std::fs::write(&users, "x").unwrap();
        sweep_parts(dir.path(), &prefix);
        assert!(!tmp.exists(), "the step's leftover goes");
        assert!(other_step.exists() && users.exists(), "nothing else");
    }

    /// The file is in place even when its temporary file can't be removed
    /// afterwards: the export counts as written, with a notice.
    #[test]
    fn a_finished_export_counts_when_its_temp_file_stays() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join(".x.part");
        let dest = dir.path().join("x.csv");
        std::fs::write(&tmp, "a,b").unwrap();
        assert_eq!(finish_export(&tmp, &dest, false).unwrap(), None);
        assert!(dest.exists() && !tmp.exists());
        // Replacing: a rename, nothing left.
        std::fs::write(&tmp, "c,d").unwrap();
        assert_eq!(finish_export(&tmp, &dest, true).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "c,d");
    }

    #[test]
    fn only_reads_are_run_again() {
        let none = HashMap::new();
        // Reads.
        for (sql, kind) in [
            ("select 1", Postgres),
            ("select * from orders where id > 3; select count(*) from customers", Postgres),
            ("with x as (select 1 as a) select a from x", Mysql),
            ("select * from results.orders", Duckdb),
            (r#"{"find": "orders", "filter": {}}"#, Mongodb),
        ] {
            assert!(reads_only(sql, kind, &none), "{sql}");
        }
        // Writes, anything unsure, and reads calling a function that may write.
        for (sql, kind) in [
            ("insert into t values (1)", Postgres),
            ("select 1; delete from t", Postgres),
            ("create table t as select 1", Duckdb),
            ("call refresh_stats()", Postgres),
            ("select archive_old_rows()", Postgres),
            ("select 1 into new_table", Postgres),
            ("", Postgres),
            ("vacuum", Postgres),
            (r#"{"delete": "orders", "deletes": [{"q": {}, "limit": 0}]}"#, Mongodb),
        ] {
            assert!(!reads_only(sql, kind, &none), "{sql}");
        }
        // With the run's values in place: a missing value is not a read.
        let p = HashMap::from([("id".to_string(), ParamValue { value: "7".into(), raw: false })]);
        assert!(reads_only("select * from t where id = :id", Postgres, &p));
        assert!(!reads_only("select * from t where id = :other", Postgres, &p));
        let raw = HashMap::from([("q".to_string(), ParamValue { value: "1; drop table t".into(), raw: true })]);
        assert!(!reads_only("select :q", Postgres, &raw));
    }

    #[test]
    fn only_connection_errors_are_retried() {
        assert!(retryable(&EngineError::new("connection", "connection reset by peer")));
        for kind in ["query", "auth", "timeout", "cancelled", "invalid", "internal"] {
            assert!(!retryable(&EngineError::new(kind, "x")), "{kind}");
        }
    }
}
