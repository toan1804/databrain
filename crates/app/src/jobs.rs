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
//! (`JobNode::ai_sql`), and every step of a run under the AI rules (one the
//! assistant started, or a "run after" job started by such a run), runs as
//! its own queries, whoever starts the run: each step's SQL, a load's
//! before/after SQL and its write (the DuckDB sandbox on DuckDB targets),
//! after the connection's AI rules as they are now. SQL DataBrain writes
//! for a step (an export's COPY, a load's INSERTs) is the job's.

use futures::FutureExt;
use std::collections::{BTreeMap, HashMap, HashSet};

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::sql::{MarkType, ParamValue};
use std::sync::Arc;
use std::time::{Duration, Instant};

use databrain_query_engine::{EngineError, EventHub, LoadDryRun, LoadEvent, LoadObserver, LoadSpec, QueryEngine, Result, RunRequest};
use databrain_workspace::jobs::Due;
use databrain_workspace::jobs::LOG_KEPT;
use databrain_workspace::{
    FileFormat, Job, JobNode, JobNodeKind, JobRun, NodeRunRecord, NodeRunSummary, Origin, PendingMark, RunLogEntry, StepProgress, Workspace, duration_text,
    now_ms,
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
/// Why a run stopped when its lease couldn't be renewed.
const LEASE_UNRENEWED: &str =
    "Stopped: the job's lease could not be renewed (the workspace kept failing), so the run stopped before another runner could take the job";

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

/// What the heartbeat of a run does after trying to renew its lease.
#[derive(Debug, PartialEq, Eq)]
enum Beat {
    /// Renewed.
    Held,
    /// The workspace failed (busy, locked): not a lost lease; the run goes
    /// on while the lease still holds.
    Going,
    /// Another runner holds it (the lease ran out, e.g. while the computer
    /// slept): stop, so it doesn't run twice.
    Lost,
    /// The workspace kept failing until the lease would run out before the
    /// next try: stop before another runner could take the job.
    Unrenewed,
}

/// The heartbeat's decision from [`renew_lease`]'s answer and the time since
/// the lease was last renewed.
fn beat(renewed: Option<bool>, since: Duration) -> Beat {
    match renewed {
        Some(true) => Beat::Held,
        Some(false) => Beat::Lost,
        None if since + LEASE_RENEW >= Duration::from_millis(LEASE_MS as u64) => Beat::Unrenewed,
        None => Beat::Going,
    }
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
    /// Runs started in this process, (job id, run id), in order: what the
    /// background runner reports (never another process's runs).
    started: Mutex<Vec<(String, i64)>>,
    /// Why a running job was stopped by the runner itself (lease lost,
    /// lease not renewed, the background runner's deadline): its run's error.
    stop_reasons: Mutex<HashMap<String, String>>,
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
            started: Mutex::default(),
            stop_reasons: Mutex::default(),
        }
    }
    /// The runs started in this process since the last call, (job id, run id).
    pub fn take_started(&self) -> Vec<(String, i64)> {
        std::mem::take(&mut *self.started.lock())
    }
    /// Stop a running job for `why` (recorded as its run's error).
    pub fn stop_for(&self, job_id: &str, why: &str) -> bool {
        self.stop_reasons.lock().entry(job_id.to_string()).or_insert_with(|| why.to_string());
        self.cancel(job_id)
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
    /// Mark `job_id` as running in this process, or not (`false`), without a run (tests).
    #[doc(hidden)]
    pub fn hold_for_tests(&self, job_id: &str, hold: bool) {
        let mut r = self.running.lock();
        if hold {
            r.insert(job_id.to_string(), CancellationToken::new());
        } else {
            r.remove(job_id);
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
/// The kind of connection `id` (the Results DuckDB when it isn't a saved one).
fn connection_kind(ctx: &JobCtx, id: &str) -> ConnectorKind {
    ctx.workspace.get_connection(id).map(|p| p.config.kind).unwrap_or(ConnectorKind::Duckdb)
}

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
    /// An incremental step's `:watermark`: its value, and the type of its
    /// column (a stored mark) or none (the start value, typed by the user):
    /// written as a literal of that type for each connection it runs on.
    watermark: Arc<Option<(String, Option<MarkType>)>>,
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
            watermark: Arc::new(None),
            load: Arc::default(),
        }
    }

    /// The run's `:name` values for a step's SQL.
    fn params(&self) -> Result<HashMap<String, ParamValue>> {
        self.params.as_ref().clone().map_err(|e| EngineError::new("invalid", e))
    }

    /// The run's `:name` values for SQL that runs on a `kind` connection,
    /// with the step's `:watermark` as a literal of its column's type there.
    fn params_for(&self, kind: ConnectorKind) -> Result<HashMap<String, ParamValue>> {
        let mut p = self.params()?;
        if let Some((value, ty)) = self.watermark.as_ref() {
            let v = match ty {
                Some(ty) => ParamValue { value: ty.literal(value, kind), raw: true },
                None => ParamValue { value: value.clone(), raw: false },
            };
            p.insert(WATERMARK.to_string(), v);
        }
        type_run_moment(&mut p, kind);
        Ok(p)
    }

    /// This tracer with the step's `:watermark`.
    fn with_watermark(&self, mark: (String, Option<MarkType>)) -> Self {
        Tracer { watermark: Arc::new(Some(mark)), ..self.clone() }
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
        let live = self.live.lock();
        if node.ai_sql || live.run.trigger == "ai" || live.run.ai { Origin::Ai } else { Origin::Job }
    }

    /// A run under the AI rules that nobody approved: a "run after" start
    /// from a run the assistant started (the user approved that run, not
    /// this job's steps one by one, and can't be asked now).
    fn unattended_ai(&self) -> bool {
        let live = self.live.lock();
        live.run.ai && live.run.trigger != "ai"
    }

    /// Set this step's progress; the run is sent at most every [`PROGRESS_EVERY`]
    /// (always when the work is done).
    fn progress(&self, p: StepProgress) {
        let Some((id, _)) = &self.node else { return };
        let snapshot = {
            let mut l = self.live.lock();
            // A total of 0 is unknown, not done.
            let finished = p.total > 0 && p.done >= p.total;
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

    /// The step's phase ended: no progress until the next one says its own.
    fn clear_progress(&self) {
        let Some((id, _)) = &self.node else { return };
        let mut l = self.live.lock();
        if let Some(r) = l.run.nodes.iter_mut().find(|r| &r.node_id == id) {
            r.summary.progress = None;
        }
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
            LoadEvent::Progress { phase, done, total, elapsed_ms } => self.progress(StepProgress { phase, done, total, elapsed_ms }),
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
        let here = ctx.engine.outputs().resolve(&n.name).is_some_and(|o| o.tab_id == tab && o.state != databrain_query_engine::OutputState::Evicted);
        // An incremental step whose pending mark came with rows this output
        // doesn't hold (another process read them, or an older run): it
        // runs again, or the steps after it would load other rows and its
        // mark would skip the pending ones.
        here && incremental_column(n).is_none_or(|col| match ctx.workspace.job_watermark_pending(&job.id, &n.id, &col) {
            Ok(Some(p)) => holds_pending_rows(ctx, job, n, &p),
            _ => true,
        })
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
    start_as(ctx, job_id, trigger, only, over, false)
}

/// [`start_with`]; `ai`: every step runs under the AI rules ([`JobRun::ai`]),
/// as in a run the assistant started: a "run after" start from such a run.
fn start_as(ctx: &JobCtx, job_id: &str, trigger: &str, only: Option<Vec<String>>, over: Option<RunParams>, ai: bool) -> Result<JobRun> {
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
    let mut run = run;
    if ai {
        // Recorded before any step runs: if it can't be, the run doesn't start.
        if let Err(e) = ctx.workspace.set_job_run_ai(run.id) {
            ctx.jobs.started.lock().push((job_id.to_string(), run.id));
            run.status = "error".into();
            run.error = Some(format!("could not record that it runs under the AI rules: {e}"));
            run.finished_at = Some(now_ms());
            let _ = ctx.workspace.update_job_run(&run);
            ctx.jobs.running.lock().remove(job_id);
            let _ = ctx.workspace.release_job(job_id, &owner);
            return Err(e.into());
        }
        run.ai = true;
    }
    let _ = ctx.workspace.touch_job_run(run.id, &owner);
    // A missing set doesn't stop the start: the run's steps fail with why,
    // so a scheduled run reports it like any failure.
    let params = effective_params(&ctx.workspace, &job, over.as_ref()).and_then(|(mut p, set)| {
        fill_run_moment(&job, &mut p, run.started_at)?;
        Ok((p, set))
    });
    if let Ok((p, _)) = &params {
        let _ = ctx.workspace.set_job_run_params(run.id, p);
        run.params = p.clone();
    }
    ctx.jobs.started.lock().push((job_id.to_string(), run.id));
    tracing::info!(job = job_id, run = run.id, trigger, steps = only.as_ref().map_or(job.nodes.len(), Vec::len), "job run started");
    emit(ctx, &run);
    let c = ctx.clone();
    let r = run.clone();
    tokio::spawn(async move {
        let id = r.job_id.clone();
        let run_id = r.id;
        // Keep the lease and tell other processes the run is alive.
        let beat = {
            let (ws, id, owner, stop, jobs) = (c.workspace.clone(), id.clone(), c.jobs.owner.clone(), cancel.clone(), c.jobs.clone());
            tokio::spawn(async move {
                let mut renewed = Instant::now();
                loop {
                    tokio::time::sleep(LEASE_RENEW).await;
                    let why = match beat(renew_lease(&ws, &id, &owner).await, renewed.elapsed()) {
                        Beat::Held => {
                            renewed = Instant::now();
                            None
                        }
                        Beat::Going => None,
                        Beat::Lost => Some(LEASE_LOST),
                        Beat::Unrenewed => Some(LEASE_UNRENEWED),
                    };
                    if let Some(why) = why {
                        tracing::warn!(job = %id, run = run_id, why, "stopping this run");
                        jobs.stop_reasons.lock().entry(id.clone()).or_insert_with(|| why.to_string());
                        stop.cancel();
                        break;
                    }
                    let _ = ws.touch_job_run(run_id, &owner);
                }
            })
        };
        let (whole, name) = (only.is_none() || r.trigger == "resume", job.name.clone());
        // The jobs after a run the assistant started run under the AI rules too.
        let ai = r.trigger == "ai" || r.ai;
        // While it runs, its steps' latest outputs stay in memory (later
        // steps read them); afterwards they make room for other jobs'.
        let running = JobRunning::new(&c, &id);
        execute(&c, job, r, only, cancel, params).await;
        drop(running);
        beat.abort();
        c.jobs.stop_reasons.lock().remove(&id);
        let _ = c.workspace.release_job(&id, &c.jobs.owner);
        // Before this job leaves `running`, so a runner waiting for it waits for them too.
        if whole && c.workspace.job_runs(&id, 1).ok().and_then(|r| r.into_iter().next()).is_some_and(|r| r.id == run_id && r.status == "success") {
            start_after(&c, &id, &name, ai);
        }
        c.jobs.running.lock().remove(&id);
    });
    Ok(run)
}

/// Trigger of a run started because another job succeeded: `after:<its name>`.
pub const AFTER_TRIGGER: &str = "after:";

/// Start the jobs that run after `job_id` (it just succeeded as a whole).
/// One that is running (here or in another process) is queued: the next
/// scheduler pass, in either process, starts it once it is free.
/// `ai`: the run that succeeded ran under the AI rules (the assistant
/// started it, or it ran after one it started): so do these.
fn start_after(ctx: &JobCtx, job_id: &str, job_name: &str, ai: bool) {
    for (id, _) in ctx.workspace.jobs_after(job_id).unwrap_or_default() {
        let trigger = format!("{AFTER_TRIGGER}{job_name}");
        // For the date the run it follows was for.
        match start_as(ctx, &id, &trigger, None, moment_of_last_success(ctx, job_id), ai) {
            Ok(_) => tracing::info!(job = %id, after = job_id, "job started after the job it follows succeeded"),
            // Running (here or elsewhere), or the workspace failed for a moment:
            // queued, so the start isn't lost.
            Err(e) if transient(&e) => {
                tracing::info!(job = %id, after = job_id, error = %e.message, "job to run after another can't start now: queued for the next scheduler pass");
                if let Err(q) = ctx.workspace.queue_job_after(&id, &trigger, ai) {
                    tracing::warn!(job = %id, error = %q, "job to run after another could not be queued");
                }
            }
            Err(e) => tracing::warn!(job = %id, after = job_id, error = %e.message, "job to run after another did not start"),
        }
    }
}

/// A start that may work later: the job is running (here or in another
/// process), or the workspace failed (busy, locked). Not: no steps, a
/// deleted job.
fn transient(e: &EngineError) -> bool {
    matches!(e.kind.as_str(), "busy" | "internal")
}

/// Start the queued "run after" starts ([`start_after`]) whose job is free
/// now. Returns the jobs started.
fn start_queued_after(ctx: &JobCtx) -> Vec<String> {
    let mut started = Vec::new();
    for (id, _) in ctx.workspace.queued_job_after().unwrap_or_default() {
        if ctx.jobs.is_running(&id) || ctx.workspace.job_lease(&id).ok().flatten().is_some_and(|(_, until)| until > now_ms()) {
            continue;
        }
        // Taken first, so only one process starts it; put back if it can't start yet.
        let Ok(Some((trigger, ai))) = ctx.workspace.take_job_after(&id) else { continue };
        // For the date the run it follows was for (that job's latest success).
        let after = trigger.strip_prefix(AFTER_TRIGGER).unwrap_or_default();
        let moment = ctx.workspace.list_jobs().unwrap_or_default().into_iter().find(|j| j.name == after).and_then(|j| moment_of_last_success(ctx, &j.id));
        match start_as(ctx, &id, &trigger, None, moment, ai) {
            Ok(_) => started.push(id),
            Err(e) if transient(&e) => {
                let _ = ctx.workspace.queue_job_after(&id, &trigger, ai);
            }
            Err(e) => tracing::warn!(job = %id, error = %e.message, "queued job to run after another did not start"),
        }
    }
    started
}

/// For a failed run nobody started by hand: the failed steps' connections
/// that sign in through the browser (or a device code), whose sign-in ends
/// and can't be renewed without someone at the computer.
fn sign_in_warning(ctx: &JobCtx, job: &Job, trigger: &str, failed: impl Fn(&str) -> bool) -> Option<String> {
    if !unattended(trigger) {
        return None;
    }
    let mut names: Vec<String> = Vec::new();
    for n in job.nodes.iter().filter(|n| failed(&n.id)) {
        for c in [&n.connection_id, &n.target_connection_id].into_iter().flatten() {
            if let Ok(p) = ctx.workspace.get_connection(c)
                && p.config.auth.is_interactive()
                && !names.contains(&p.name)
            {
                names.push(p.name);
            }
        }
    }
    (!names.is_empty()).then(|| {
        format!(
            "{} sign{} in through the browser: when the sign-in has ended, a run nobody watches can't renew it. Sign in again in DataBrain, or use a sign-in that doesn't need a browser (a key, token or service account) for scheduled jobs",
            names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", "),
            if names.len() == 1 { "s" } else { "" }
        )
    })
}

/// Runs started with nobody at the computer (schedule, background runner, after another job).
fn unattended(trigger: &str) -> bool {
    matches!(trigger, "schedule" | "background") || trigger.starts_with(AFTER_TRIGGER)
}

/// The job counts as running for the outputs' memory budget while held
/// ([`databrain_query_engine::OutputRegistry::job_running`]).
struct JobRunning(Arc<databrain_query_engine::OutputRegistry>, String);

impl JobRunning {
    fn new(ctx: &JobCtx, job_id: &str) -> Self {
        let outputs = ctx.engine.outputs().clone();
        outputs.job_running(job_id, true);
        JobRunning(outputs, job_id.to_string())
    }
}

impl Drop for JobRunning {
    fn drop(&mut self) {
        self.0.job_running(&self.1, false);
    }
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
        "schedule" => "on its schedule".to_string(),
        "ai" => "by the AI assistant (approved)".into(),
        "background" => "on its schedule, by the background runner (DataBrain closed)".into(),
        "cli" => "from the command line (databrain-jobs)".into(),
        "resume" => "by hand, resuming the steps that did not succeed".into(),
        t => match t.strip_prefix(AFTER_TRIGGER) {
            Some(up) if run.ai => format!("after job \"{up}\" succeeded (a run the AI assistant started: every step runs under the AI rules)"),
            Some(up) => format!("after job \"{up}\" succeeded"),
            None => "by hand".into(),
        },
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
    // Incremental steps' new marks, kept once the steps after them succeed.
    let mut marks: HashMap<String, PendingMark> = HashMap::new();
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
                    ctx.jobs.stop_reasons.lock().entry(job.id.clone()).or_insert_with(|| LEASE_LOST.to_string());
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
                if let Some((column, value, ty)) = out.mark {
                    // Taken now: a step after it that succeeds from now on has its rows.
                    marks
                        .insert(node.id.clone(), PendingMark { column, value, value_type: Some(ty.as_str().into()), run_id: Some(run.id), taken_at: now_ms() });
                }
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
    // Earlier runs' outputs of the steps: no step reads them (they read the
    // latest), and the user opens the latest ("View output").
    let released: usize = job.nodes.iter().map(|n| ctx.engine.outputs().release_earlier(&node_tab(&job.id, &n.id))).sum();
    if released > 0 {
        ctx.engine.outputs_changed();
    }
    advance_marks(ctx, &job, &status, &marks, &tracer);
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
    if final_status == "error"
        && let Some(w) = sign_in_warning(ctx, &job, &run.trigger, |id| status.get(id) == Some(&"error"))
    {
        tracer.log("warning", w, None, None, None);
    }
    let nodes = tracer.with_run(|run| {
        run.status = final_status.into();
        run.error = match run_limit.filter(|_| timed_out) {
            Some(d) => Some(format!("Stopped: {}", timed_out_text(d))),
            None => run.nodes.iter().find(|r| r.summary.status == "error").map(|r| format!("{}: {}", r.name, r.summary.error.clone().unwrap_or_default())),
        };
        // Stopped by the runner itself (lease lost or not renewed, the
        // background runner's deadline): say why.
        if final_status == "cancelled"
            && run.error.is_none()
            && let Some(why) = ctx.jobs.stop_reasons.lock().get(&job.id).cloned()
        {
            run.error = Some(why);
        }
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
    let Ok(mut params) = t.params() else { return Some("the run's parameter values are missing".into()) };
    // An incremental step's SQL reads `:watermark` too (a value, never SQL):
    // any value classifies it the same (an empty one counts as missing).
    if incremental_column(node).is_some() {
        params.entry(WATERMARK.to_string()).or_insert(ParamValue { value: "0".into(), raw: false });
    }
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
    let params = t.params_for(connection_kind(ctx, &connection_id))?;
    t.log("info", format!("Running SQL on {conn}"), None, None, None);
    // Rows read so far: a large table isn't just "running".
    let reading = |done: u64, elapsed_ms: u64| t.progress(StepProgress { phase: "Reading rows".into(), done, total: 0, elapsed_ms });
    let outcomes = ctx
        .engine
        .run_and_watch(
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
            &reading,
        )
        .await;
    // Read: what comes next (a load) says its own progress.
    t.clear_progress();
    let outcomes = outcomes?;
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
    /// Incremental: the largest value of its column now (column, value,
    /// its type), kept as its mark when the steps after it succeed too.
    mark: Option<(String, String, MarkType)>,
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
    // Incremental: the step's SQL (and a load's before/after SQL) gets its
    // mark as `:watermark`.
    let incremental = incremental_column(node);
    let with_mark;
    let t = match &incremental {
        Some(col) => {
            with_mark = t.with_watermark(watermark_now(ctx, job_id, node, col, t)?);
            &with_mark
        }
        None => t,
    };
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
        ai_rules_now(ctx, job_id, node, &sql, &connection_id, t).await?;
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
        let mark = match &incremental {
            Some(col) => {
                let output =
                    ctx.engine.outputs().for_tab(&tab).into_iter().rev().find(|o| o.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&node.name)));
                let output = output.ok_or_else(|| {
                    EngineError::new("invalid", format!("The SQL returned no rows to take the incremental column {col} from (it must end with a SELECT)"))
                })?;
                incremental_max(ctx, &output, col).await?
            }
            None => None,
        };
        return Ok(NodeOut { rows, file: None, notices: vec![], mark });
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
    // Taken before anything is written, so a missing column fails first.
    let mark = match &incremental {
        Some(col) => incremental_max(ctx, &output, col).await?,
        None => None,
    };
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
        return Ok(NodeOut { rows: output.rows as i64, file: Some(path.to_string_lossy().into_owned()), notices, mark });
    }
    let target =
        node.target_connection_id.as_deref().filter(|c| !c.is_empty()).ok_or_else(|| EngineError::new("invalid", "Choose the connection to load into"))?;
    let spec = load_spec_now(ctx, node, target, origin, t)?;
    // Rows of an incremental step are only the new ones: a mode that
    // removes the table's other rows would keep only them.
    if let Some(inc) = ctx.workspace.get_job(job_id).ok().and_then(|j| incremental_upstream(&j, &node.id).map(|n| n.name.clone())) {
        use databrain_workspace::LoadMode;
        match node.load_mode {
            LoadMode::Sync => {
                return Err(EngineError::new(
                    "invalid",
                    format!(
                        "Sync deletes the rows of the table that aren't among the step's, and the rows come from incremental step {inc} (only those after its last run's): it would delete everything loaded before. Use Merge, or turn Incremental off."
                    ),
                ));
            }
            LoadMode::Truncate | LoadMode::Replace => t.log(
                "warning",
                format!(
                    "The table keeps only this run's rows: they come from incremental step {inc} (only those after its last run's), and {} removes the others",
                    mode_name(node.load_mode)
                ),
                None,
                None,
                None,
            ),
            _ => {}
        }
    }
    // From here a failure may have written: a retry needs the load to say it kept nothing.
    *t.load.lock() = LoadState::Started;
    let report = ctx.engine.write_output(&output.handle, target, &step_load(&tab), &spec, cancel, t).await?;
    Ok(NodeOut { rows: report.rows as i64, file: None, notices: report.notices, mark })
}

/// [`load_spec`] with the run's `:name` values in the before/after SQL.
fn load_spec_now(ctx: &JobCtx, node: &JobNode, target: &str, origin: Origin, t: &Tracer) -> Result<LoadSpec> {
    let mut spec = load_spec(node);
    // Under the AI rules (a run it started, the assistant's SQL or rows),
    // the user's before/after SQL runs as the assistant's too (sandboxed on
    // a DuckDB target), as the step's own SQL.
    spec.origin = origin;
    let kind = ctx.workspace.get_connection(target)?.config.kind;
    let params = t.params_for(kind)?;
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

/// The parameter an incremental step's SQL reads its mark from.
pub const WATERMARK: &str = "watermark";

/// The date a run is for, in the job's time zone: a scheduled run's
/// scheduled time (each missed one its own, with "run each"), the run it
/// follows for "run after", the run it resumes for Resume, else when it
/// started; or the date given ("Run for date", `--param run_date=…`).
/// SQL that filters on it (`where day = :run_date - 1`) reads the same
/// rows however late the run is, which `current_date` doesn't.
pub const RUN_DATE: &str = "run_date";
/// [`RUN_DATE`] with its time of day (the scheduled time; midnight of a
/// date given without one).
pub const RUN_TIME: &str = "run_time";

/// `:run_date` and `:run_time` of a run for the time `at_ms` (epoch ms).
pub fn run_moment(job: &Job, at_ms: i64) -> BTreeMap<String, ParamValue> {
    let (date, time) = job.schedule.local_date_time(at_ms);
    BTreeMap::from([(RUN_DATE.to_string(), ParamValue { value: date, raw: false }), (RUN_TIME.to_string(), ParamValue { value: time, raw: false })])
}

/// Gives a run's values the `:run_date` / `:run_time` they lack (the run
/// is for `now`, or midnight of a `run_date` given alone), and checks the
/// ones given: a date `2026-11-16`, a time `2026-11-16 06:30[:00]`. A value
/// typed as an expression (`{}` in the box) is left as it is.
fn fill_run_moment(job: &Job, values: &mut BTreeMap<String, ParamValue>, now: i64) -> std::result::Result<(), String> {
    use chrono::{NaiveDate, NaiveDateTime};
    let date = match values.get(RUN_DATE) {
        None => {
            for (k, v) in run_moment(job, now) {
                values.entry(k).or_insert(v);
            }
            return Ok(());
        }
        Some(v) if v.raw => None,
        Some(v) => Some(
            NaiveDate::parse_from_str(v.value.trim(), "%Y-%m-%d")
                .map_err(|_| format!(":{RUN_DATE} must be a date like 2026-11-16, not \"{}\"", v.value.trim()))?,
        ),
    };
    match values.get(RUN_TIME) {
        None => {
            if let Some(d) = date {
                values.insert(RUN_TIME.to_string(), ParamValue { value: format!("{d} 00:00:00"), raw: false });
            }
        }
        Some(v) if v.raw => {}
        Some(v) => {
            let t = v.value.trim();
            let parsed = NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S").or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M"));
            let t = parsed.map_err(|_| format!(":{RUN_TIME} must be a date and time like 2026-11-16 06:30:00, not \"{t}\""))?;
            values.insert(RUN_TIME.to_string(), ParamValue { value: t.format("%Y-%m-%d %H:%M:%S").to_string(), raw: false });
        }
    }
    Ok(())
}

/// `:run_date` / `:run_time` as date and timestamp literals of a `kind`
/// connection (`DATE '2026-11-16'`), so `:run_date - 1` is date arithmetic
/// there, not text.
fn type_run_moment(params: &mut HashMap<String, ParamValue>, kind: ConnectorKind) {
    for (name, ty) in [(RUN_DATE, MarkType::Date), (RUN_TIME, MarkType::Timestamp)] {
        if let Some(v) = params.get_mut(name).filter(|v| !v.raw) {
            *v = ParamValue { value: ty.literal(&v.value, kind), raw: true };
        }
    }
}

/// What `:run_date` and `:run_time` are in a step's SQL, for a run for a
/// date: a user writing `where booking_date = :run_date - 1` sees the value
/// its database gets, and the SQL as it would run.
#[derive(Debug, Clone, serde::Serialize, ts_rs::TS)]
#[ts(export)]
pub struct RunMomentPreview {
    /// The run's date and time (`2026-11-18`, `2026-11-18 06:30:00`): the
    /// one asked for, else the job's (its parameter set's) `run_date`, else
    /// now in the job's time zone.
    pub date: String,
    pub time: String,
    /// The step's SQL, then a load's before and after SQL (on the target),
    /// those it has.
    pub parts: Vec<RunMomentPart>,
}

/// One SQL text of a step, on the database it runs on.
#[derive(Debug, Clone, serde::Serialize, ts_rs::TS)]
#[ts(export)]
pub struct RunMomentPart {
    #[ts(type = "\"step\" | \"before\" | \"after\"")]
    pub what: String,
    /// Where it runs: a query step's connection; a load's or export's own
    /// SQL on Results (DuckDB); before/after SQL on the load's target.
    pub kind: ConnectorKind,
    /// `:run_date` is text there (SQLite, MongoDB, Neo4j), not a date.
    pub text: bool,
    /// As that database gets them (`DATE '2026-11-18'`).
    pub run_date: String,
    pub run_time: String,
    /// The SQL with them in (other parameters as written).
    pub sql: String,
    /// Ways to write the day before there, and what each gives.
    pub examples: Vec<RunDateExample>,
}

/// An expression of `:run_date` in a database's SQL and the value it gives.
#[derive(Debug, Clone, PartialEq, serde::Serialize, ts_rs::TS)]
#[ts(export)]
pub struct RunDateExample {
    /// "the day before", "the day before as text"…
    pub label: String,
    /// With `:run_date` (`to_char(:run_date - 1, 'DD-Mon-YY')`).
    pub sql: String,
    /// What it gives for the preview's date (`17-Nov-26`).
    pub value: String,
}

/// The day before `:run_date` in `kind`'s SQL, then as text in three
/// common shapes, each with what it gives for `date`. None where dates
/// aren't SQL values (MongoDB, Neo4j). Checked against live servers in
/// `run_date_examples_give_what_they_say` (tests/jobs.rs).
pub fn run_date_examples(kind: ConnectorKind, date: chrono::NaiveDate) -> Vec<RunDateExample> {
    use ConnectorKind as K;
    let day_before = match kind {
        K::Postgres | K::Oracle | K::Duckdb | K::Clickhouse => ":run_date - 1",
        K::Mysql => ":run_date - interval 1 day",
        K::Mssql | K::Snowflake => "dateadd(day, -1, :run_date)",
        K::Bigquery => "date_sub(:run_date, interval 1 day)",
        K::Databricks => "date_sub(:run_date, 1)",
        K::Sqlite => "date(:run_date, '-1 day')",
        _ => return vec![],
    };
    // (`2026-11-17`, `17-Nov-26`, `20261117`) in the database's own patterns.
    let text = |iso: bool, dmy: bool| -> Option<String> {
        let y = day_before;
        let pick = |a: &str, b: &str, c: &str| {
            Some(
                (if iso {
                    a
                } else if dmy {
                    b
                } else {
                    c
                })
                .to_string(),
            )
        };
        match kind {
            K::Postgres | K::Oracle => pick("YYYY-MM-DD", "DD-Mon-YY", "YYYYMMDD").map(|p| format!("to_char({y}, '{p}')")),
            K::Snowflake => pick("YYYY-MM-DD", "DD-MON-YY", "YYYYMMDD").map(|p| format!("to_char({y}, '{p}')")),
            K::Mysql => pick("%Y-%m-%d", "%d-%b-%y", "%Y%m%d").map(|p| format!("date_format({y}, '{p}')")),
            // `convert` styles, not `format` (it needs the CLR, which
            // Azure SQL Edge lacks, and is slow): 23 `2026-11-17`, 6
            // `17 Nov 26`, 112 `20261117`.
            K::Mssql if iso => Some(format!("convert(varchar(10), {y}, 23)")),
            K::Mssql if dmy => Some(format!("replace(convert(varchar(9), {y}, 6), ' ', '-')")),
            K::Mssql => Some(format!("convert(varchar(8), {y}, 112)")),
            K::Clickhouse => pick("%Y-%m-%d", "%d-%b-%y", "%Y%m%d").map(|p| format!("formatDateTime({y}, '{p}')")),
            K::Duckdb => pick("%Y-%m-%d", "%d-%b-%y", "%Y%m%d").map(|p| format!("strftime({y}, '{p}')")),
            K::Bigquery => pick("%Y-%m-%d", "%d-%b-%y", "%Y%m%d").map(|p| format!("format_date('{p}', {y})")),
            K::Databricks => pick("yyyy-MM-dd", "dd-MMM-yy", "yyyyMMdd").map(|p| format!("date_format({y}, '{p}')")),
            // No month names in SQLite.
            K::Sqlite if dmy => None,
            K::Sqlite => Some(format!("strftime('{}', {y})", if iso { "%Y-%m-%d" } else { "%Y%m%d" })),
            _ => None,
        }
    };
    let before = date.pred_opt().unwrap_or(date);
    let mut out = vec![RunDateExample { label: "the day before".into(), sql: day_before.into(), value: before.format("%Y-%m-%d").to_string() }];
    for (iso, dmy, shape) in [(true, false, "%Y-%m-%d"), (false, true, "%d-%b-%y"), (false, false, "%Y%m%d")] {
        if let Some(sql) = text(iso, dmy) {
            out.push(RunDateExample { label: "the day before, as text".into(), sql, value: before.format(shape).to_string() });
        }
    }
    out
}

/// [`RunMomentPreview`] of step `node_id` of `job` (as edited), for a run
/// for `date` (`2026-11-18`): the values a run would get, the run's date
/// as the job's parameters set it otherwise ([`effective_params`]), else now.
pub fn run_moment_preview(ctx: &JobCtx, job: &Job, node_id: &str, date: Option<&str>) -> Result<RunMomentPreview> {
    let node = job.nodes.iter().find(|n| n.id == node_id).ok_or_else(|| EngineError::new("not_found", "No such step"))?;
    // As a run builds them: the set's, the job's, then the run's (the date asked for).
    let (all, _) = effective_params(&ctx.workspace, job, None).unwrap_or_default();
    let mut values: BTreeMap<String, ParamValue> = all.into_iter().filter(|(k, _)| k == RUN_DATE || k == RUN_TIME).collect();
    if let Some(d) = date.map(str::trim).filter(|d| !d.is_empty()) {
        values.insert(RUN_DATE.to_string(), ParamValue { value: d.to_string(), raw: false });
        values.remove(RUN_TIME);
    }
    fill_run_moment(job, &mut values, now_ms()).map_err(|e| EngineError::new("invalid", e))?;
    let plain = |k: &str| values.get(k).map(|v| v.value.clone()).unwrap_or_default();
    let (date, time) = (plain(RUN_DATE), plain(RUN_TIME));
    let day = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok();
    let step_kind = match (&node.kind, node.connection_id.as_deref()) {
        (JobNodeKind::Query, Some(c)) if !c.is_empty() => connection_kind(ctx, c),
        _ => ConnectorKind::Duckdb,
    };
    // Before/after SQL run on the target: its kind, not Results'.
    let target = node.target_connection_id.as_deref().filter(|c| node.kind == JobNodeKind::Load && !c.is_empty());
    let mut texts = vec![("step", step_kind, node.sql.as_str())];
    if let Some(t) = target {
        let kind = connection_kind(ctx, t);
        texts.push(("before", kind, node.load_before_sql.as_deref().unwrap_or_default()));
        texts.push(("after", kind, node.load_after_sql.as_deref().unwrap_or_default()));
    }
    let parts = texts
        .into_iter()
        .filter(|(_, _, sql)| !sql.trim().is_empty())
        .map(|(what, kind, sql)| {
            let mut typed: HashMap<String, ParamValue> = values.clone().into_iter().collect();
            type_run_moment(&mut typed, kind);
            let literal = |k: &str| typed.get(k).map(|v| v.value.clone()).unwrap_or_default();
            RunMomentPart {
                what: what.into(),
                kind,
                text: matches!(kind, ConnectorKind::Sqlite | ConnectorKind::Mongodb | ConnectorKind::Neo4j),
                run_date: literal(RUN_DATE),
                run_time: literal(RUN_TIME),
                sql: databrain_connector_core::sql::substitute_known_parameters(sql, kind, &typed),
                examples: day.map(|d| run_date_examples(kind, d)).unwrap_or_default(),
            }
        })
        .collect();
    Ok(RunMomentPreview { date, time, parts })
}

/// The `:run_date` / `:run_time` of `job_id`'s latest successful run, for
/// a run that follows it ("run after").
fn moment_of_last_success(ctx: &JobCtx, job_id: &str) -> Option<RunParams> {
    let run = ctx.workspace.job_runs(job_id, 10).ok()?.into_iter().find(|r| r.status == "success")?;
    let values: BTreeMap<String, ParamValue> = run.params.into_iter().filter(|(k, _)| k == RUN_DATE || k == RUN_TIME).collect();
    (!values.is_empty()).then_some(RunParams { set: None, values })
}

/// An incremental step's column, if it has one.
/// The incremental step whose rows step `id` gets (itself, or a step
/// above it), if any.
fn incremental_upstream<'j>(job: &'j Job, id: &str) -> Option<&'j JobNode> {
    let mut todo = vec![id.to_string()];
    let mut seen = HashSet::new();
    while let Some(id) = todo.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(n) = job.nodes.iter().find(|n| n.id == id).filter(|n| incremental_column(n).is_some()) {
            return Some(n);
        }
        todo.extend(job.edges.iter().filter(|e| e.to == id).map(|e| e.from.clone()));
    }
    None
}

/// A load mode as the editor names it.
fn mode_name(m: databrain_workspace::LoadMode) -> &'static str {
    use databrain_workspace::LoadMode;
    match m {
        LoadMode::Append => "Append",
        LoadMode::Truncate => "Truncate",
        LoadMode::Replace => "Replace",
        LoadMode::Update => "Update",
        LoadMode::Merge => "Merge",
        LoadMode::Sync => "Sync",
    }
}

fn incremental_column(node: &JobNode) -> Option<String> {
    node.incremental_column.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(String::from)
}

/// `:watermark` for this run of an incremental step: its mark (written
/// back as its column's type), else its start value (none yet, or reset).
fn watermark_now(ctx: &JobCtx, job_id: &str, node: &JobNode, col: &str, t: &Tracer) -> Result<(String, Option<MarkType>)> {
    if let Some(w) = ctx.workspace.job_watermark(job_id, &node.id, col)? {
        let from = w.run_id.map(|r| format!(" from run #{r}")).unwrap_or_default();
        t.log("info", format!("Incremental on {col}: :{WATERMARK} = {} (the step's mark{from})", w.value), None, None, None);
        return Ok((w.value, w.value_type.as_deref().and_then(MarkType::parse)));
    }
    match node.incremental_start.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(start) => {
            t.log("info", format!("Incremental on {col}: :{WATERMARK} = {start} (the start value: the step has no mark yet)"), None, None, None);
            Ok((start.to_string(), None))
        }
        None => Err(EngineError::new(
            "invalid",
            format!("Enter the start value of the incremental column {col}: the step's SQL gets it as :{WATERMARK} until the step has a mark"),
        )),
    }
}

/// The largest value of column `col` in `output` (all its rows, those on
/// disk too; NULL and NaN left out), with the column's name and type; None
/// when it has no such value. (The result stays locked while its files on
/// disk are read: the grid and the outputs list wait meanwhile.)
async fn incremental_max(ctx: &JobCtx, output: &databrain_query_engine::OutputInfo, col: &str) -> Result<Option<(String, String, MarkType)>> {
    // Part of the rows: their largest value would skip the rest next time.
    if output.truncated {
        return Err(EngineError::new(
            "invalid",
            format!(
                "{} holds only its first {} rows (capped by a row limit or by the memory a result may keep): the incremental mark can't be taken from part of them. Select fewer rows per run (e.g. a smaller range after :{WATERMARK}).",
                output.name.as_deref().unwrap_or(&output.handle),
                output.rows
            ),
        ));
    }
    let rs = ctx.engine.results().get(&output.result_id).map_err(|e| EngineError::new("internal", e.to_string()))?;
    let col = col.to_string();
    tokio::task::spawn_blocking(move || {
        let mut g = rs.lock();
        let schema = g.schema();
        let Some(i) = schema.fields().iter().position(|f| f.name().eq_ignore_ascii_case(&col)) else {
            let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
            return Err(EngineError::new("invalid", format!("The step's rows have no column {col} (its incremental column); they have {}", names.join(", "))));
        };
        let max = g.column_max(i).map_err(|e| EngineError::new("internal", format!("reading the largest {col}: {e}")))?;
        let field = schema.field(i);
        let ty = MarkType::of(field, databrain_result_store::display::is_decimal_text(field));
        Ok(max.map(|v| (field.name().clone(), v, ty)))
    })
    .await
    .map_err(|_| EngineError::new("internal", "reading the incremental mark stopped unexpectedly"))?
}

/// Incremental marks after a run. A step that succeeded keeps its new mark
/// when every step after it (all downstream, transitively) succeeded in
/// this run too; otherwise the mark waits ("pending") and the next run reads
/// the same rows again, unless a later run that doesn't rerun the step
/// (a resume, "Run this step" or "Run from here" below it) leaves every
/// step after it succeeded since the mark was taken (in that run, or in
/// one that ended after the mark was taken): then the pending mark becomes
/// the mark, as its rows are loaded everywhere. A step's success from
/// before the mark was taken doesn't count: it loaded older rows. A rerun
/// of the step replaces its pending mark; one that fails drops it.
fn advance_marks(ctx: &JobCtx, job: &Job, status: &HashMap<String, &'static str>, marks: &HashMap<String, PendingMark>, tracer: &Tracer) {
    // Every step after `id` (transitively), in no particular order.
    let below = |id: &str| -> Vec<&JobNode> {
        let mut out: Vec<&JobNode> = Vec::new();
        let mut todo = vec![id.to_string()];
        let mut seen = HashSet::new();
        while let Some(id) = todo.pop() {
            for e in job.edges.iter().filter(|e| e.from == id) {
                if seen.insert(e.to.clone())
                    && let Some(n) = job.nodes.iter().find(|x| x.id == e.to)
                {
                    out.push(n);
                    todo.push(e.to.clone());
                }
            }
        }
        out
    };
    // The first step after `id` that hasn't succeeded since `taken_at`:
    // in this run, or (not in it) in its last run, ended after it.
    let blocked_after = |id: &str, taken_at: i64| -> Option<String> {
        below(id)
            .into_iter()
            .find(|n| match status.get(&n.id) {
                Some(s) => *s != "success",
                None => !n.last_run.as_ref().is_some_and(|r| r.status == "success" && r.finished_at >= taken_at),
            })
            .map(|n| n.name.clone())
    };
    for n in &job.nodes {
        let Some(col) = incremental_column(n) else { continue };
        let t = tracer.for_node(n);
        match status.get(&n.id) {
            Some(&"success") => {
                let mark = marks.get(&n.id);
                if let Some(step) = blocked_after(&n.id, mark.map_or(0, |m| m.taken_at)).filter(|_| mark.is_some()).or_else(|| {
                    // Without a new mark only the steps of this run matter.
                    below(&n.id).into_iter().find(|x| status.get(&x.id).is_some_and(|s| *s != "success")).map(|x| x.name.clone())
                }) {
                    let _ = ctx.workspace.set_job_watermark_pending(&job.id, &n.id, mark);
                    t.log(
                        "warning",
                        format!(
                            "Incremental mark not moved: step {step} after it didn't succeed in this run, so the next run reads these rows again{}",
                            if mark.is_some() { " (a resume that makes it succeed moves it)" } else { "" }
                        ),
                        None,
                        None,
                        None,
                    );
                    continue;
                }
                match mark {
                    Some(m) => match ctx.workspace.set_job_watermark(&job.id, &n.id, &m.column, &m.value, m.value_type.as_deref(), m.run_id) {
                        Ok(()) => t.log("success", format!("Incremental mark moved: largest {} is now {}", m.column, m.value), None, None, None),
                        Err(e) => {
                            t.log("error", format!("The incremental mark could not be saved ({e}): the next run reads these rows again"), None, None, None)
                        }
                    },
                    None => {
                        let _ = ctx.workspace.set_job_watermark_pending(&job.id, &n.id, None);
                        t.log("info", format!("Incremental mark kept: no rows with a {col} this run"), None, None, None)
                    }
                }
            }
            // It ran and failed: its pending rows aren't the ones its output holds now.
            Some(&"error") => {
                let _ = ctx.workspace.set_job_watermark_pending(&job.id, &n.id, None);
            }
            // Skipped, or stopped (it may not have started): a pending mark
            // stays; it moves only with the output it was taken with
            // ([`holds_pending_rows`]).
            Some(_) => {}
            // Not in this run: a pending mark moves once every step after it
            // has succeeded since the mark was taken, one of them in this run.
            None => {
                let Ok(Some(p)) = ctx.workspace.job_watermark_pending(&job.id, &n.id, &col) else { continue };
                let after = below(&n.id);
                if !after.iter().any(|x| status.contains_key(&x.id)) || blocked_after(&n.id, p.taken_at).is_some() {
                    continue;
                }
                // The steps after it read its output in this process: only
                // when that is the output the mark was taken with were the
                // pending rows loaded. Otherwise they weren't: dropped, the
                // next run reads them again.
                if !holds_pending_rows(ctx, job, n, &p) {
                    let _ = ctx.workspace.set_job_watermark_pending(&job.id, &n.id, None);
                    t.log(
                        "warning",
                        format!(
                            "Incremental mark not moved: the steps after it read an output of {} other than the one run{} read its rows into (it ran in another process, or earlier), so the next run reads those rows again",
                            n.name,
                            p.run_id.map(|r| format!(" #{r}")).unwrap_or_default()
                        ),
                        None,
                        None,
                        None,
                    );
                    continue;
                }
                match ctx.workspace.set_job_watermark(&job.id, &n.id, &p.column, &p.value, p.value_type.as_deref(), p.run_id) {
                    Ok(()) => t.log(
                        "success",
                        format!(
                            "Incremental mark moved: the steps after it succeeded, so the rows it read{} are loaded; largest {} is now {}",
                            p.run_id.map(|r| format!(" in run #{r}")).unwrap_or_default(),
                            p.column,
                            p.value
                        ),
                        None,
                        None,
                        None,
                    ),
                    Err(e) => t.log("error", format!("The incremental mark could not be saved ({e}): the next run reads these rows again"), None, None, None),
                }
            }
        }
    }
}

/// Whether step `n`'s output in this process holds the rows its pending
/// mark `p` was taken from: created by the run that took it (from its
/// start to the mark). An output of another process (the background
/// runner, `databrain-jobs run`) isn't here; an older one predates the run.
fn holds_pending_rows(ctx: &JobCtx, job: &Job, n: &JobNode, p: &PendingMark) -> bool {
    let Some(run) = p.run_id.and_then(|r| ctx.workspace.job_run(&job.id, r).ok().flatten()) else { return false };
    let tab = node_tab(&job.id, &n.id);
    ctx.engine.outputs().resolve(&n.name).is_some_and(|o| {
        o.tab_id == tab && o.state != databrain_query_engine::OutputState::Evicted && o.created_at >= run.started_at && o.created_at <= p.taken_at
    })
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
        insert_only: node.load_insert_only,
    }
}

/// A step's SQL as it will run (`sql`, with the run's `:name` values in
/// place where they are known), checked like the assistant's own queries
/// ([`databrain_ai::jobs::review_step`]). What only the user could approve
/// was approved with the job and is not asked again. The table rules
/// complete unqualified names with the schema each connection's server
/// says is current, asked now: `databrain-jobs tick` is a new process each
/// time, and in the app what was learned before may be out of date.
async fn ai_rules_now(ctx: &JobCtx, job_id: &str, node: &JobNode, sql: &str, connection_id: &str, t: &Tracer) -> Result<()> {
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
    let filled = |sql: &str, connection: Option<&str>| -> String {
        let kind = connection.and_then(|c| ctx.workspace.get_connection(c).ok()).map(|p| p.config.kind);
        kind.and_then(|k| databrain_connector_core::sql::substitute_parameters(sql, k, &t.params_for(k).unwrap_or_default()).ok())
            .unwrap_or_else(|| sql.to_string())
    };
    let mut checked = node.clone();
    checked.sql = filled(sql, Some(connection_id));
    let target = node.target_connection_id.clone();
    for s in [&mut checked.load_before_sql, &mut checked.load_after_sql].into_iter().flatten() {
        *s = filled(s, target.as_deref());
    }
    let mut confirm = databrain_ai::jobs::review_step(&ctx.engine, &checked).map_err(|e| {
        let why = match e {
            databrain_ai::AiError::Policy(r) => r,
            other => other.to_string(),
        };
        EngineError::new(
            "invalid",
            format!("Not run: the connection's AI settings no longer allow what the assistant wrote ({why}). See the connection's AI settings."),
        )
    })?;
    // What only the user may approve (a function that could write, rows
    // copied out of masked columns): approved with the job or the run the
    // assistant started, but a job started after that run was approved as
    // a whole, not step by step, and nobody can be asked now: refused (the
    // run approval said so), never dropped.
    if t.unattended_ai() {
        if let Ok(job) = ctx.workspace.get_job(job_id) {
            confirm.extend(databrain_ai::jobs::copy_confirms(&ctx.engine, &job, node));
        }
        if !confirm.is_empty() {
            return Err(EngineError::new(
                "invalid",
                format!(
                    "Not run: this job started after a run the assistant started, and this step needs your approval, which can't be asked now ({}). Run the job yourself to run it.",
                    confirm.join("; ")
                ),
            ));
        }
    }
    Ok(())
}

/// Whose SQL a step's is outside a run (dry runs, loads): the AI's when it
/// wrote it, else the job's.
fn sql_origin(node: &JobNode) -> Origin {
    if node.ai_sql { Origin::Ai } else { Origin::Job }
}

/// Rows a dry run selects to try the load with.
pub const DRY_RUN_ROWS: usize = 1000;

/// `:name` values a dry run selects its rows with: the job's, and an
/// incremental step's `:watermark` as its next run would get it.
fn dry_run_params(ctx: &JobCtx, job: &Job, node: &JobNode) -> HashMap<String, ParamValue> {
    let mut params: BTreeMap<String, ParamValue> = effective_params(&ctx.workspace, job, None).map(|(p, _)| p).unwrap_or_default();
    // For today, as a run started now; the rows are selected on DuckDB.
    let _ = fill_run_moment(job, &mut params, now_ms());
    let mut params: HashMap<String, ParamValue> = params.into_iter().collect();
    type_run_moment(&mut params, ConnectorKind::Duckdb);
    if let Some(col) = incremental_column(node) {
        // The rows are selected on DuckDB: the mark as a DuckDB literal of its type.
        let mark = ctx.workspace.job_watermark(&job.id, &node.id, &col).ok().flatten().map(|w| match w.value_type.as_deref().and_then(MarkType::parse) {
            Some(ty) => ParamValue { value: ty.literal(&w.value, ConnectorKind::Duckdb), raw: true },
            None => ParamValue { value: w.value, raw: false },
        });
        let start = || node.incremental_start.clone().filter(|s| !s.trim().is_empty()).map(|v| ParamValue { value: v, raw: false });
        if let Some(v) = mark.or_else(start) {
            params.insert(WATERMARK.to_string(), v);
        }
    }
    params
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
                params: dry_run_params(ctx, job, node),
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
    let owner = ctx.jobs.owner.clone();
    let mut started = start_queued_after(ctx);
    for j in ctx.workspace.list_jobs().unwrap_or_default() {
        // Per job: the queued starts and earlier jobs take time.
        let now = now_ms();
        if !j.schedule.enabled || ctx.jobs.is_running(&j.id) || j.next_run_at.is_none_or(|t| t > now) {
            continue;
        }
        // Take the job, then look again: another process may have just run it.
        if !ctx.workspace.claim_job(&j.id, &owner, now_ms() + LEASE_MS).unwrap_or(false) {
            continue;
        }
        let release = || let_go(ctx, &j.id);
        let fresh = ctx.workspace.get_job(&j.id).ok();
        let base = fresh.as_ref().map(|f| f.schedule_base_at(now));
        // Missed runs: one for all (marked now), none, or one per missed time
        // (marked at that time, so the next pass finds the next one due).
        let mark = match fresh.as_ref().map_or(Due::No, |f| f.schedule.due(base.unwrap_or(now), now)) {
            Due::Run { mark } => mark,
            Due::No => {
                release();
                continue;
            }
            // Skip is for times missed while nothing ran the scheduler: a time
            // missed because the job was still running (a run by hand, a slow
            // run, a "run after" chain) runs once it ends.
            Due::Skip { mark } if fresh.as_ref().is_some_and(|f| ran_at_its_time(ctx, f, now)) => mark,
            Due::Skip { mark } => {
                tracing::info!(job = %j.id, "scheduled runs missed while nothing was running are skipped (the job's setting)");
                let _ = ctx.workspace.mark_job_scheduled(&j.id, mark);
                release();
                continue;
            }
        };
        // The run is for its scheduled time (`:run_date`), however late it starts.
        let moment = fresh.as_ref().and_then(|f| {
            let slot = f.schedule.slot_for(mark, base.unwrap_or(now), now)?;
            Some(RunParams { set: None, values: run_moment(f, slot) })
        });
        // Marked while the lease is held (a fast run could otherwise end and
        // release it before the mark, and another runner start it again)…
        let before = fresh.and_then(|f| f.last_scheduled_at);
        // …and not started unmarked: it would be due again as soon as it
        // ended, and run over and over while marking keeps failing.
        if let Err(e) = ctx.workspace.mark_job_scheduled(&j.id, mark) {
            tracing::warn!(job = %j.id, error = %e, "scheduled job not started: its start could not be recorded; tried again on the next tick");
            release();
            continue;
        }
        match start_with(ctx, &j.id, trigger, None, moment) {
            Ok(_) => started.push(j.id),
            Err(e) => give_back(ctx, &j.id, mark, before, &e),
        }
    }
    started
}

/// Whether one of `job`'s recent runs was going at a scheduled time not yet
/// counted (missed for being busy, not while nothing was running).
fn ran_at_its_time(ctx: &JobCtx, job: &Job, now: i64) -> bool {
    let base = job.schedule_base_at(now);
    ctx.workspace.job_runs(&job.id, 5).unwrap_or_default().iter().any(|r| {
        let end = r.finished_at.unwrap_or(now).min(now);
        job.schedule.next_after(base.max(r.started_at - 1)).is_some_and(|slot| slot <= end)
    })
}

/// Let go of a job the scheduler claimed and didn't start: unless a run of
/// the job started in this process meanwhile (a job that runs after
/// another, between the scheduler's check and its claim), which holds the
/// lease (same owner) and must keep it.
#[doc(hidden)]
pub fn let_go(ctx: &JobCtx, job_id: &str) {
    release_unless_running(ctx, job_id);
}

/// Release this process's lease on `job_id` unless a run of it started
/// here: checked and released under the `running` lock, so a run starting
/// meanwhile ([`start_as`] adds itself there before it claims the lease)
/// can't have its lease released under it.
fn release_unless_running(ctx: &JobCtx, job_id: &str) {
    let running = ctx.jobs.running.lock();
    if !running.contains_key(job_id) {
        let _ = ctx.workspace.release_job(job_id, &ctx.jobs.owner);
    }
}

/// A scheduled start that failed after the job was claimed and marked
/// `mark`. A start that may work later gives the slot back (the next tick
/// tries again) and the lease; except:
/// - a run of the job started in this process meanwhile (a job that runs
///   after another) holds the lease (same owner) and uses the slot;
/// - another process holds the job ("busy"): it ran or runs it, the slot
///   is used;
/// - a start that can't work (no steps, the job is gone): the slot stays
///   used, so it isn't tried again every tick;
/// - another runner marked the slot meanwhile (the stored mark isn't ours,
///   or our lease is gone): left as it is ([`Workspace::give_back_job_slot`]).
#[doc(hidden)]
pub fn give_back(ctx: &JobCtx, job_id: &str, mark: i64, before: Option<i64>, e: &EngineError) {
    if ctx.jobs.is_running(job_id) {
        tracing::info!(job = %job_id, "scheduled job is already running here (started after another job): this slot is used by that run");
        return;
    }
    if e.kind == "busy" {
        tracing::info!(job = %job_id, error = %e.message, "scheduled job runs in another process: this slot is used there");
        return;
    }
    if !transient(e) {
        tracing::warn!(job = %job_id, error = %e.message, "scheduled job can't start; it is tried at its next time");
        release_unless_running(ctx, job_id);
        return;
    }
    tracing::warn!(job = %job_id, error = %e.message, "scheduled job did not start; it is tried again on the next tick");
    match ctx.workspace.give_back_job_slot(job_id, &ctx.jobs.owner, mark, before) {
        Ok(true) => {}
        Ok(false) => tracing::info!(job = %job_id, "the slot was used by another runner meanwhile: left as it is"),
        Err(err) => tracing::warn!(job = %job_id, error = %err, "the slot could not be given back"),
    }
    release_unless_running(ctx, job_id);
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

/// Wait until no job runs in this process, also those started meanwhile
/// (jobs that run after one that succeeded). Returns every job seen
/// running, in the order first seen.
pub async fn wait_all(ctx: &JobCtx) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    loop {
        let now = ctx.jobs.running_ids();
        for id in &now {
            if !seen.contains(id) {
                seen.push(id.clone());
            }
        }
        if now.is_empty() {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::ConnectorKind::{Duckdb, Mongodb, Mysql, Postgres};

    /// A run gets `:run_date` / `:run_time` when it has none; a date given
    /// alone is for its midnight; values that aren't a date (or a date and
    /// time) are refused; an expression is left as typed. Bound as a date
    /// and timestamp literal of each database.
    #[test]
    fn run_dates_are_checked_and_typed() {
        let job = Job {
            id: String::new(),
            name: "j".into(),
            nodes: vec![],
            edges: vec![],
            schedule: databrain_workspace::JobSchedule { time_zone: "Asia/Ho_Chi_Minh".into(), ..Default::default() },
            last_scheduled_at: None,
            params: Default::default(),
            param_set: None,
            limits: Default::default(),
            created_at: 0,
            updated_at: 0,
        };
        let v = |s: &str| ParamValue { value: s.into(), raw: false };
        let get = |p: &BTreeMap<String, ParamValue>| (p[RUN_DATE].value.clone(), p[RUN_TIME].value.clone());
        // 2026-11-16 20:00 UTC is the 17th, 03:00, in Ho Chi Minh City.
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-16T20:00:00Z").unwrap().timestamp_millis();
        let mut p = BTreeMap::new();
        fill_run_moment(&job, &mut p, at).unwrap();
        assert_eq!(get(&p), ("2026-11-17".into(), "2026-11-17 03:00:00".into()));
        let mut p = BTreeMap::from([(RUN_DATE.to_string(), v(" 2026-11-01 "))]);
        fill_run_moment(&job, &mut p, at).unwrap();
        assert_eq!(p[RUN_TIME].value, "2026-11-01 00:00:00");
        let mut p = BTreeMap::from([(RUN_DATE.to_string(), v("2026-11-01")), (RUN_TIME.to_string(), v("2026-11-01 06:30"))]);
        fill_run_moment(&job, &mut p, at).unwrap();
        assert_eq!(p[RUN_TIME].value, "2026-11-01 06:30:00");
        for bad in ["yesterday", "2026-13-01", "16/11/2026", ""] {
            let mut p = BTreeMap::from([(RUN_DATE.to_string(), v(bad))]);
            assert!(fill_run_moment(&job, &mut p, at).is_err_and(|e| e.contains(":run_date must be a date")), "{bad:?}");
        }
        let mut p = BTreeMap::from([(RUN_DATE.to_string(), v("2026-11-01")), (RUN_TIME.to_string(), v("6:30"))]);
        assert!(fill_run_moment(&job, &mut p, at).is_err_and(|e| e.contains(":run_time")));
        let expr = ParamValue { value: "current_date - 1".into(), raw: true };
        let mut p = BTreeMap::from([(RUN_DATE.to_string(), expr.clone())]);
        fill_run_moment(&job, &mut p, at).unwrap();
        assert_eq!(p[RUN_DATE], expr);

        let typed = |kind| {
            let mut p: HashMap<String, ParamValue> = [(RUN_DATE.to_string(), v("2026-11-16")), (RUN_TIME.to_string(), v("2026-11-16 06:30:00"))].into();
            type_run_moment(&mut p, kind);
            (p[RUN_DATE].value.clone(), p[RUN_TIME].value.clone(), p[RUN_DATE].raw)
        };
        assert_eq!(typed(Postgres), ("DATE '2026-11-16'".into(), "TIMESTAMP '2026-11-16 06:30:00'".into(), true));
        assert_eq!(typed(ConnectorKind::Mssql).0, "CAST('2026-11-16' AS date)");
        assert_eq!(typed(ConnectorKind::Sqlite).0, "'2026-11-16'");
        // An expression stays as typed.
        let mut p: HashMap<String, ParamValue> = [(RUN_DATE.to_string(), expr.clone())].into();
        type_run_moment(&mut p, Postgres);
        assert_eq!(p[RUN_DATE], expr);
    }

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
    fn the_heartbeat_stops_only_when_it_must() {
        let lease = Duration::from_millis(LEASE_MS as u64);
        assert_eq!(beat(Some(true), lease * 2), Beat::Held);
        // Another runner holds it: lost, at once.
        assert_eq!(beat(Some(false), Duration::ZERO), Beat::Lost);
        // A busy workspace is not a lost lease: the run goes on while the lease holds…
        assert_eq!(beat(None, Duration::ZERO), Beat::Going);
        assert_eq!(beat(None, lease - LEASE_RENEW - Duration::from_secs(1)), Beat::Going);
        // …and stops before it could run out (the next try would be too late).
        assert_eq!(beat(None, lease - LEASE_RENEW), Beat::Unrenewed);
        assert_eq!(beat(None, lease), Beat::Unrenewed);
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
        // An incremental step's `:watermark` (bound for the check as a stand-in value).
        let w = HashMap::from([(WATERMARK.to_string(), ParamValue { value: "0".into(), raw: false })]);
        for kind in [databrain_connector_core::ConnectorKind::Sqlite, Postgres, Duckdb] {
            assert!(reads_only("select 42 as id where 42 > :watermark", kind, &w), "{kind:?}");
            assert!(!reads_only("delete from t where id < :watermark; select 1 as id", kind, &w), "{kind:?}");
        }
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
