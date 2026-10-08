//! Jobs: a graph of SQL steps (nodes) with dependencies (edges), run in
//! order by the app, by hand or on a schedule, with a run history.
//!
//! A job is stored as one JSON document (nodes, edges, schedule), like
//! notebooks. Runs are rows of `job_runs` with each node's outcome.

use chrono::{Datelike, Duration, Local, NaiveTime, TimeZone};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Workspace, new_id, now_ms};

/// Run history kept per job.
pub const RUNS_KEPT: i64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum JobNodeKind {
    /// Run `sql` on `connection_id`; the result is the output `name`.
    #[default]
    Query,
    /// Run `sql` on DuckDB (it reads upstream outputs as `results.<name>`)
    /// and write the rows into `target_table` of `target_connection_id`.
    Load,
    /// Run `sql` on DuckDB and write the rows to a file in `export_folder`
    /// (DuckDB `COPY … TO`).
    Export,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    #[default]
    Csv,
    Parquet,
    /// One JSON object per line.
    Json,
}

impl FileFormat {
    pub fn extension(self) -> &'static str {
        match self {
            FileFormat::Csv => "csv",
            FileFormat::Parquet => "parquet",
            FileFormat::Json => "json",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LoadMode {
    /// Insert the rows. A missing table is created from the rows' columns
    /// when `create_table` is on.
    #[default]
    Append,
    /// Delete the table's rows, then insert (missing table: as `Append`).
    Truncate,
    /// Drop the table (if it exists), create it from the rows' columns, insert.
    Replace,
    /// Update rows of an existing table whose `key_columns` match; rows
    /// without a match are ignored.
    Update,
    /// Update rows whose `key_columns` match and insert the others (upsert);
    /// the table must exist.
    Merge,
}

impl LoadMode {
    /// Modes matching rows by `key_columns`.
    pub fn needs_keys(self) -> bool {
        matches!(self, LoadMode::Update | LoadMode::Merge)
    }
    /// Modes writing into a table that must already exist.
    pub fn needs_table(self) -> bool {
        self.needs_keys()
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NodeRunSummary {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub finished_at: i64,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub rows: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    /// File written by an export step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Things the user should know (a load step created its table, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
    /// While running: rows written so far out of the total (load steps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<StepProgress>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StepProgress {
    /// What is being done ("Inserting rows", "Staging rows").
    pub phase: String,
    pub done: u64,
    pub total: u64,
}

/// One line of a run's log, for tracing what ran and what came back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RunLogEntry {
    /// Order in the run (from 1).
    pub seq: u64,
    /// Epoch ms.
    pub at: i64,
    /// Step it belongs to (`None` for the run itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// `info`, `success`, `warning`, `error`.
    pub level: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

/// Lines kept per run log (the rest is summarised in one line).
pub const LOG_KEPT: usize = 5000;
/// Longest SQL text kept per log line.
pub const LOG_SQL_CHARS: usize = 8000;

/// `sql` cut to [`LOG_SQL_CHARS`].
pub fn log_sql(sql: &str) -> String {
    if sql.len() <= LOG_SQL_CHARS {
        return sql.to_string();
    }
    let mut end = LOG_SQL_CHARS;
    while !sql.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} … ({} more characters)", &sql[..end], sql.len() - end)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobNode {
    pub id: String,
    /// Output name: downstream DuckDB steps read it as `results.<name>`.
    pub name: String,
    #[serde(default)]
    pub kind: JobNodeKind,
    /// Where `sql` runs (Query). Load steps run on the Results DuckDB.
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub sql: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub target_connection_id: Option<String>,
    #[serde(default)]
    pub target_table: Option<String>,
    #[serde(default)]
    pub load_mode: LoadMode,
    /// Load: SQL run on the target connection before the rows are written
    /// (one or more statements).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_before_sql: Option<String>,
    /// Load: SQL run on the target connection after the rows are written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_after_sql: Option<String>,
    /// Load (Update/Merge): columns identifying a row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_columns: Vec<String>,
    /// Load (Append/Truncate): create a missing table from the rows' columns.
    #[serde(default = "yes")]
    pub create_table: bool,
    /// Load: rows per INSERT statement (`None` = the connection's default;
    /// kept within its maximum).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_rows: Option<u32>,
    /// Load: most KB of SQL text per INSERT statement (`None` = default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_kb: Option<u32>,
    /// Export: folder of the file (empty = Downloads).
    #[serde(default)]
    pub export_folder: Option<String>,
    /// Export: file name, `{step}`, `{job}`, `{date}`, `{time}` replaced
    /// (empty = `{step}_{date}_{time}`); the extension is added when missing.
    #[serde(default)]
    pub export_file: Option<String>,
    #[serde(default)]
    pub export_format: FileFormat,
    #[serde(default)]
    pub last_run: Option<NodeRunSummary>,
}

impl JobNode {
    /// A step does one thing: drop the settings of the other actions, so a
    /// query step can't also carry a load target or a file name.
    pub fn keep_own_settings(&mut self) {
        let load = self.kind == JobNodeKind::Load;
        let export = self.kind == JobNodeKind::Export;
        if self.kind != JobNodeKind::Query {
            // Load and export steps select their rows on DuckDB.
            self.connection_id = None;
        }
        if !load {
            self.target_connection_id = None;
            self.target_table = None;
            self.load_mode = LoadMode::default();
            self.load_before_sql = None;
            self.load_after_sql = None;
            self.key_columns.clear();
            self.create_table = true;
            self.batch_rows = None;
            self.batch_kb = None;
        } else if !self.load_mode.needs_keys() {
            self.key_columns.clear();
        }
        if !export {
            self.export_folder = None;
            self.export_file = None;
            self.export_format = FileFormat::default();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEdge {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    /// Every `minutes`.
    #[default]
    Interval,
    /// At `at` (local "HH:MM") on `weekdays` (0 = Sunday; empty = every day).
    Daily,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSchedule {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: ScheduleMode,
    #[serde(default = "default_minutes")]
    pub minutes: u32,
    #[serde(default = "default_at")]
    pub at: String,
    #[serde(default)]
    pub weekdays: Vec<u8>,
}

fn default_minutes() -> u32 {
    60
}
fn default_at() -> String {
    "08:00".into()
}

impl Default for JobSchedule {
    fn default() -> Self {
        Self { enabled: false, mode: ScheduleMode::Interval, minutes: default_minutes(), at: default_at(), weekdays: Vec::new() }
    }
}

impl JobSchedule {
    /// First run time strictly after `after_ms` (epoch ms), `None` when off
    /// or invalid. Daily times are local time.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        if !self.enabled {
            return None;
        }
        match self.mode {
            ScheduleMode::Interval => {
                let step = i64::from(self.minutes.max(1)) * 60_000;
                // Aligned to the interval (every 15 min → :00, :15, …), local time.
                let offset = Local.timestamp_millis_opt(after_ms).single()?.offset().local_minus_utc() as i64 * 1000;
                let local = after_ms + offset;
                Some((local / step + 1) * step - offset)
            }
            ScheduleMode::Daily => {
                let time = NaiveTime::parse_from_str(self.at.trim(), "%H:%M").ok()?;
                let start = Local.timestamp_millis_opt(after_ms).single()?;
                for d in 0..8 {
                    let day = start.date_naive() + Duration::days(d);
                    if !self.weekdays.is_empty() && !self.weekdays.contains(&(day.weekday().num_days_from_sunday() as u8)) {
                        continue;
                    }
                    // A time skipped by a DST change runs an hour later.
                    let at = Local
                        .from_local_datetime(&day.and_time(time))
                        .earliest()
                        .or_else(|| Local.from_local_datetime(&(day.and_time(time) + Duration::hours(1))).earliest())?;
                    let ms = at.timestamp_millis();
                    if ms > after_ms {
                        return Some(ms);
                    }
                }
                None
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub nodes: Vec<JobNode>,
    #[serde(default)]
    pub edges: Vec<JobEdge>,
    #[serde(default)]
    pub schedule: JobSchedule,
    /// When the scheduler last started it (epoch ms).
    #[serde(default)]
    pub last_scheduled_at: Option<i64>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

impl Job {
    /// Time the next scheduled run is counted from: the last scheduled
    /// start, or the last edit (turning a schedule on doesn't run at once).
    pub fn schedule_base(&self) -> i64 {
        self.last_scheduled_at.unwrap_or(self.updated_at).max(self.updated_at)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: String,
    pub name: String,
    pub node_count: usize,
    /// Output names its steps own.
    pub step_names: Vec<String>,
    pub schedule: JobSchedule,
    pub next_run_at: Option<i64>,
    pub last_run: Option<JobRun>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRunRecord {
    pub node_id: String,
    pub name: String,
    #[serde(flatten)]
    pub summary: NodeRunSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRun {
    pub id: i64,
    pub job_id: String,
    /// "manual" or "schedule".
    pub trigger: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// "running", "success", "error", "cancelled".
    pub status: String,
    pub nodes: Vec<NodeRunRecord>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Spec {
    #[serde(default)]
    nodes: Vec<JobNode>,
    #[serde(default)]
    edges: Vec<JobEdge>,
    #[serde(default)]
    schedule: JobSchedule,
}

/// Output names are SQL identifiers (`results.<name>`), same rules as
/// other outputs: at most 63 characters, no `__` (versions, `x__1`), not a
/// handle (`r12`).
pub fn valid_node_name(name: &str) -> bool {
    let mut chars = name.chars();
    let ident = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    let handle = name.len() > 1 && name.starts_with(['r', 'R']) && name[1..].chars().all(|c| c.is_ascii_digit());
    ident && name.len() <= 63 && !name.contains("__") && !handle
}

/// The job step that owns an output name: only that step's runs may
/// produce `results.<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOwner {
    pub job_id: String,
    pub job_name: String,
    pub node_id: String,
    pub step: String,
}

impl StepOwner {
    /// Run key of the step (`RunRequest.tab_id` of its runs).
    pub fn tab(&self) -> String {
        format!("job:{}:{}", self.job_id, self.node_id)
    }
    pub fn describe(&self) -> String {
        format!("step \"{}\" of job \"{}\"", self.step, self.job_name)
    }
}

/// Nodes in run order (each after its upstreams), or the nodes of a cycle.
pub fn topo_order(job: &Job) -> std::result::Result<Vec<String>, Vec<String>> {
    let ids: Vec<&str> = job.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut indeg: std::collections::HashMap<&str, usize> = ids.iter().map(|i| (*i, 0)).collect();
    for e in &job.edges {
        if let Some(d) = indeg.get_mut(e.to.as_str()) {
            if ids.contains(&e.from.as_str()) {
                *d += 1;
            }
        }
    }
    let mut ready: Vec<&str> = ids.iter().copied().filter(|i| indeg[i] == 0).collect();
    let mut out = Vec::new();
    while let Some(n) = (!ready.is_empty()).then(|| ready.remove(0)) {
        out.push(n.to_string());
        for e in job.edges.iter().filter(|e| e.from == n) {
            if let Some(d) = indeg.get_mut(e.to.as_str()) {
                *d -= 1;
                if *d == 0 {
                    ready.push(e.to.as_str());
                }
            }
        }
    }
    if out.len() == ids.len() {
        Ok(out)
    } else {
        Err(ids.iter().filter(|i| !out.iter().any(|o| o == *i)).map(|s| s.to_string()).collect())
    }
}

fn validate(job: &Job) -> Result<()> {
    if job.name.trim().is_empty() {
        return Err(Error::Invalid("job name is required".into()));
    }
    let mut names = std::collections::HashSet::new();
    for n in &job.nodes {
        if !valid_node_name(&n.name) {
            return Err(Error::Invalid(format!(
                "step name \"{}\" must be letters, digits and _ (max 63, no \"__\", not like r12): it is the output name",
                n.name
            )));
        }
        if !names.insert(n.name.to_ascii_lowercase()) {
            return Err(Error::Invalid(format!("two steps are named \"{}\"", n.name)));
        }
    }
    if let Err(cycle) = topo_order(job) {
        let names: Vec<&str> = job.nodes.iter().filter(|n| cycle.contains(&n.id)).map(|n| n.name.as_str()).collect();
        return Err(Error::Invalid(format!("steps depend on each other in a loop: {}", names.join(", "))));
    }
    Ok(())
}

fn run_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobRun> {
    Ok(JobRun {
        id: r.get(0)?,
        job_id: r.get(1)?,
        trigger: r.get(2)?,
        started_at: r.get(3)?,
        finished_at: r.get(4)?,
        status: r.get(5)?,
        nodes: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
        error: r.get(7)?,
    })
}

const RUN_COLS: &str = "id, job_id, trigger, started_at, finished_at, status, nodes_json, error";

impl Workspace {
    pub fn list_jobs(&self) -> Result<Vec<JobSummary>> {
        let jobs: Vec<Job> = {
            let c = self.conn.lock();
            let mut stmt = c.prepare("SELECT id FROM jobs ORDER BY name COLLATE NOCASE")?;
            let ids = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            drop(c);
            ids.iter().filter_map(|id| self.get_job(id).ok()).collect()
        };
        jobs.into_iter()
            .map(|j| {
                let last_run = self.job_runs(&j.id, 1)?.into_iter().next();
                Ok(JobSummary {
                    next_run_at: j.schedule.next_after(j.schedule_base()),
                    id: j.id,
                    name: j.name,
                    node_count: j.nodes.len(),
                    step_names: j.nodes.iter().map(|n| n.name.clone()).collect(),
                    schedule: j.schedule,
                    last_run,
                    updated_at: j.updated_at,
                })
            })
            .collect()
    }

    /// Every step's output name (lowercase) with its owner.
    pub fn job_step_names(&self) -> Result<Vec<(String, StepOwner)>> {
        let ids: Vec<String> = {
            let c = self.conn.lock();
            let mut stmt = c.prepare("SELECT id FROM jobs")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out = Vec::new();
        for id in ids {
            let Ok(j) = self.get_job(&id) else { continue };
            for n in &j.nodes {
                out.push((n.name.to_ascii_lowercase(), StepOwner { job_id: j.id.clone(), job_name: j.name.clone(), node_id: n.id.clone(), step: n.name.clone() }));
            }
        }
        Ok(out)
    }

    /// The step that owns `name` (case-insensitive), if any.
    pub fn job_step_owner(&self, name: &str) -> Result<Option<StepOwner>> {
        let key = name.to_ascii_lowercase();
        Ok(self.job_step_names()?.into_iter().find(|(n, _)| *n == key).map(|(_, o)| o))
    }

    pub fn get_job(&self, id: &str) -> Result<Job> {
        let c = self.conn.lock();
        c.query_row("SELECT id, name, spec_json, last_scheduled_at, created_at, updated_at FROM jobs WHERE id = ?1", [id], |r| {
            let spec: Spec = serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
            Ok(Job {
                id: r.get(0)?,
                name: r.get(1)?,
                nodes: spec.nodes,
                edges: spec.edges,
                schedule: spec.schedule,
                last_scheduled_at: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("job {id}")))
    }

    /// Insert (empty id) or update. Nodes without ids get one; edges to
    /// missing nodes and duplicate edges are dropped.
    pub fn save_job(&self, mut job: Job) -> Result<Job> {
        for n in &mut job.nodes {
            if n.id.is_empty() {
                n.id = new_id();
            }
            n.keep_own_settings();
        }
        let ids: Vec<String> = job.nodes.iter().map(|n| n.id.clone()).collect();
        let mut edges: Vec<JobEdge> = Vec::new();
        for e in job.edges.drain(..) {
            if e.from != e.to && ids.contains(&e.from) && ids.contains(&e.to) && !edges.contains(&e) {
                edges.push(e);
            }
        }
        job.edges = edges;
        validate(&job)?;
        // Output names are unique across jobs: a step owns its name.
        for (name, owner) in self.job_step_names()? {
            if owner.job_id == job.id {
                continue;
            }
            if let Some(n) = job.nodes.iter().find(|n| n.name.to_ascii_lowercase() == name) {
                return Err(Error::Invalid(format!("results.{} is already the output of {}; choose another step name", n.name, owner.describe())));
            }
        }
        let now = now_ms();
        if job.id.is_empty() {
            job.id = new_id();
            job.created_at = now;
        }
        job.updated_at = now;
        let spec = serde_json::to_string(&Spec { nodes: job.nodes.clone(), edges: job.edges.clone(), schedule: job.schedule.clone() })
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO jobs (id, name, spec_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, spec_json = excluded.spec_json, updated_at = excluded.updated_at",
            params![job.id, job.name.trim(), spec, job.created_at, job.updated_at],
        )?;
        drop(c);
        self.get_job(&job.id)
    }

    pub fn delete_job(&self, id: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM job_runs WHERE job_id = ?1", [id])?;
        c.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Record that the scheduler started the job at `at` (not a user edit).
    pub fn mark_job_scheduled(&self, id: &str, at: i64) -> Result<()> {
        self.conn.lock().execute("UPDATE jobs SET last_scheduled_at = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    /// Save each node's last outcome into the job (without touching `updated_at`).
    pub fn set_job_node_results(&self, id: &str, results: &[NodeRunRecord]) -> Result<()> {
        let mut job = self.get_job(id)?;
        for n in &mut job.nodes {
            if let Some(r) = results.iter().find(|r| r.node_id == n.id) {
                n.last_run = Some(r.summary.clone());
            }
        }
        let spec = serde_json::to_string(&Spec { nodes: job.nodes, edges: job.edges, schedule: job.schedule }).map_err(|e| Error::Invalid(e.to_string()))?;
        self.conn.lock().execute("UPDATE jobs SET spec_json = ?2 WHERE id = ?1", params![id, spec])?;
        Ok(())
    }

    pub fn start_job_run(&self, job_id: &str, trigger: &str) -> Result<JobRun> {
        let now = now_ms();
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO job_runs (job_id, trigger, started_at, status, nodes_json) VALUES (?1, ?2, ?3, 'running', '[]')",
            params![job_id, trigger, now],
        )?;
        let id = c.last_insert_rowid();
        c.execute(
            "DELETE FROM job_runs WHERE job_id = ?1 AND id NOT IN (SELECT id FROM job_runs WHERE job_id = ?1 ORDER BY id DESC LIMIT ?2)",
            params![job_id, RUNS_KEPT],
        )?;
        Ok(JobRun { id, job_id: job_id.into(), trigger: trigger.into(), started_at: now, finished_at: None, status: "running".into(), nodes: vec![], error: None })
    }

    pub fn update_job_run(&self, run: &JobRun) -> Result<()> {
        let nodes = serde_json::to_string(&run.nodes).map_err(|e| Error::Invalid(e.to_string()))?;
        self.conn.lock().execute(
            "UPDATE job_runs SET finished_at = ?2, status = ?3, nodes_json = ?4, error = ?5 WHERE id = ?1",
            params![run.id, run.finished_at, run.status, nodes, run.error],
        )?;
        Ok(())
    }

    /// Save the whole log of a run (replaces what was saved before).
    pub fn set_job_run_log(&self, run_id: i64, log: &[RunLogEntry]) -> Result<()> {
        let json = serde_json::to_string(log).map_err(|e| Error::Invalid(e.to_string()))?;
        self.conn.lock().execute("UPDATE job_runs SET log_json = ?2 WHERE id = ?1", params![run_id, json])?;
        Ok(())
    }

    /// Log of a run of `job_id`, oldest first (empty for runs before logs existed).
    pub fn job_run_log(&self, job_id: &str, run_id: i64) -> Result<Vec<RunLogEntry>> {
        let json: Option<String> = self
            .conn
            .lock()
            .query_row("SELECT log_json FROM job_runs WHERE id = ?1 AND job_id = ?2", params![run_id, job_id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("run {run_id}")))?;
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    /// Newest first.
    pub fn job_runs(&self, job_id: &str, limit: i64) -> Result<Vec<JobRun>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(&format!("SELECT {RUN_COLS} FROM job_runs WHERE job_id = ?1 ORDER BY id DESC LIMIT ?2"))?;
        let rows = stmt.query_map(params![job_id, limit], run_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Runs left "running" by a previous app session are marked cancelled.
    pub fn close_stale_job_runs(&self) -> Result<usize> {
        Ok(self.conn.lock().execute(
            "UPDATE job_runs SET status = 'cancelled', finished_at = started_at, error = 'The app was closed while the job ran' WHERE status = 'running'",
            [],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str) -> JobNode {
        JobNode {
            id: String::new(),
            name: name.into(),
            kind: JobNodeKind::Query,
            connection_id: None,
            sql: "select 1".into(),
            x: 0.0,
            y: 0.0,
            target_connection_id: None,
            target_table: None,
            load_mode: LoadMode::Append,
            load_before_sql: None,
            load_after_sql: None,
            key_columns: vec![],
            create_table: true,
            batch_rows: None,
            batch_kb: None,
            export_folder: None,
            export_file: None,
            export_format: FileFormat::Csv,
            last_run: None,
        }
    }

    fn job(nodes: Vec<JobNode>, edges: Vec<(usize, usize)>) -> Job {
        let mut nodes = nodes;
        for (i, n) in nodes.iter_mut().enumerate() {
            n.id = format!("n{i}");
        }
        Job {
            id: String::new(),
            name: "Daily".into(),
            edges: edges.into_iter().map(|(a, b)| JobEdge { from: format!("n{a}"), to: format!("n{b}") }).collect(),
            nodes,
            schedule: JobSchedule::default(),
            last_scheduled_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn crud_order_and_validation() {
        let ws = Workspace::open_in_memory().unwrap();
        let j = ws.save_job(job(vec![node("load"), node("orders"), node("customers")], vec![(1, 0), (2, 0), (1, 0), (0, 0), (0, 9)])).unwrap();
        assert_eq!(j.edges.len(), 2, "duplicates, self-loops and dangling edges are dropped");
        assert_eq!(topo_order(&j).unwrap(), vec!["n1", "n2", "n0"]);
        assert_eq!(ws.list_jobs().unwrap()[0].node_count, 3);

        let cyclic = job(vec![node("a"), node("b")], vec![(0, 1), (1, 0)]);
        assert!(ws.save_job(cyclic).unwrap_err().to_string().contains("loop"));
        assert!(ws.save_job(job(vec![node("a b")], vec![])).is_err());
        assert!(ws.save_job(job(vec![node("a"), node("A")], vec![])).is_err());
        for bad in ["r12", "a__b", "1x"] {
            assert!(ws.save_job(job(vec![node(bad)], vec![])).is_err(), "{bad}");
        }
        // Another job can't use a name this job's steps own (any case).
        let err = ws.save_job(Job { name: "Other".into(), ..job(vec![node("Orders")], vec![]) }).unwrap_err().to_string();
        assert!(err.contains("step \"orders\" of job \"Daily\""), "{err}");
        let owner = ws.job_step_owner("ORDERS").unwrap().unwrap();
        assert_eq!((owner.job_id.as_str(), owner.node_id.as_str()), (j.id.as_str(), "n1"));
        assert_eq!(owner.tab(), format!("job:{}:n1", j.id));

        let mut run = ws.start_job_run(&j.id, "manual").unwrap();
        run.status = "success".into();
        run.finished_at = Some(run.started_at + 5);
        run.nodes = vec![NodeRunRecord { node_id: "n1".into(), name: "orders".into(), summary: NodeRunSummary { status: "success".into(), rows: Some(3), ..Default::default() } }];
        ws.update_job_run(&run).unwrap();
        ws.set_job_node_results(&j.id, &run.nodes).unwrap();
        let back = ws.get_job(&j.id).unwrap();
        assert_eq!(back.nodes[1].last_run.as_ref().unwrap().rows, Some(3));
        assert_eq!(back.updated_at, j.updated_at);
        assert_eq!(ws.job_runs(&j.id, 10).unwrap()[0].nodes[0].summary.rows, Some(3));
        assert_eq!(ws.list_jobs().unwrap()[0].last_run.as_ref().unwrap().status, "success");

        // Run log: saved whole, read back per run of its job.
        assert!(ws.job_run_log(&j.id, run.id).unwrap().is_empty());
        let line = RunLogEntry { seq: 1, at: 5, node_id: Some("n1".into()), step: Some("orders".into()), level: "info".into(), message: "Ran".into(), sql: Some("select 1".into()), rows: Some(1), duration_ms: Some(2) };
        ws.set_job_run_log(run.id, std::slice::from_ref(&line)).unwrap();
        assert_eq!(ws.job_run_log(&j.id, run.id).unwrap(), vec![line]);
        assert!(ws.job_run_log("other", run.id).is_err());
        assert!(log_sql(&"é".repeat(LOG_SQL_CHARS)).ends_with("more characters)"));

        ws.start_job_run(&j.id, "schedule").unwrap();
        assert_eq!(ws.close_stale_job_runs().unwrap(), 1);
        // The job itself can be saved again with its names.
        ws.save_job(ws.get_job(&j.id).unwrap()).unwrap();
        ws.delete_job(&j.id).unwrap();
        assert!(ws.get_job(&j.id).is_err());
        assert!(ws.job_runs(&j.id, 10).unwrap().is_empty());
    }

    #[test]
    fn steps_keep_only_their_own_action() {
        let ws = Workspace::open_in_memory().unwrap();
        let mut q = node("q");
        q.target_table = Some("t".into());
        q.export_file = Some("f.csv".into());
        q.load_before_sql = Some("delete from t".into());
        let mut l = node("l");
        l.kind = JobNodeKind::Load;
        l.connection_id = Some("pg".into());
        l.target_table = Some("t".into());
        l.key_columns = vec!["id".into()];
        l.export_folder = Some("/tmp".into());
        let mut m = node("m");
        m.kind = JobNodeKind::Load;
        m.load_mode = LoadMode::Merge;
        m.key_columns = vec!["id".into()];
        let j = ws.save_job(job(vec![q, l, m], vec![])).unwrap();
        let (q, l, m) = (&j.nodes[0], &j.nodes[1], &j.nodes[2]);
        assert_eq!((q.target_table.as_deref(), q.export_file.as_deref(), q.load_before_sql.as_deref()), (None, None, None));
        assert_eq!((l.connection_id.as_deref(), l.target_table.as_deref(), l.export_folder.as_deref()), (None, Some("t"), None));
        assert!(l.key_columns.is_empty(), "insert doesn't use keys");
        assert_eq!(m.key_columns, vec!["id"]);
    }

    #[test]
    fn schedule_next_run() {
        let off = JobSchedule::default();
        assert_eq!(off.next_after(0), None);
        let every15 = JobSchedule { enabled: true, minutes: 15, ..Default::default() };
        let t = Local.with_ymd_and_hms(2026, 3, 10, 9, 7, 0).unwrap().timestamp_millis();
        assert_eq!(every15.next_after(t), Some(Local.with_ymd_and_hms(2026, 3, 10, 9, 15, 0).unwrap().timestamp_millis()));
        let daily = JobSchedule { enabled: true, mode: ScheduleMode::Daily, at: "08:30".into(), ..Default::default() };
        assert_eq!(daily.next_after(t), Some(Local.with_ymd_and_hms(2026, 3, 11, 8, 30, 0).unwrap().timestamp_millis()));
        // 2026-03-10 is a Tuesday; Mondays only → the 16th.
        let mondays = JobSchedule { weekdays: vec![1], ..daily.clone() };
        assert_eq!(mondays.next_after(t), Some(Local.with_ymd_and_hms(2026, 3, 16, 8, 30, 0).unwrap().timestamp_millis()));
        assert_eq!(JobSchedule { at: "25:00".into(), ..daily }.next_after(t), None);
    }
}

/// File name of an export step at `now_ms`: `{step}`, `{job}`, `{date}`
/// (YYYY-MM-DD), `{time}` (HHMMSS) replaced, local time; characters not
/// allowed in file names become `_`; the format's extension is added when
/// missing. Empty = `{step}_{date}_{time}`.
pub fn export_file_name(template: Option<&str>, step: &str, job: &str, format: FileFormat, now_ms: i64) -> String {
    let t = template.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("{step}_{date}_{time}");
    let at = Local.timestamp_millis_opt(now_ms).single().unwrap_or_else(Local::now);
    let name = t
        .replace("{step}", step)
        .replace("{job}", job)
        .replace("{date}", &at.format("%Y-%m-%d").to_string())
        .replace("{time}", &at.format("%H%M%S").to_string());
    let mut clean: String = name.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c }).collect();
    clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() {
        clean = step.to_string();
    }
    let ext = format!(".{}", format.extension());
    if !clean.to_ascii_lowercase().ends_with(&ext) {
        clean.push_str(&ext);
    }
    clean
}

#[cfg(test)]
mod export_tests {
    use super::*;

    #[test]
    fn file_names() {
        let t = Local.with_ymd_and_hms(2026, 10, 8, 2, 5, 9).unwrap().timestamp_millis();
        assert_eq!(export_file_name(None, "sales", "Nightly", FileFormat::Csv, t), "sales_2026-10-08_020509.csv");
        assert_eq!(export_file_name(Some("{job} {date}"), "s", "Night/ly", FileFormat::Parquet, t), "Night_ly 2026-10-08.parquet");
        assert_eq!(export_file_name(Some("report.JSON"), "s", "j", FileFormat::Json, t), "report.JSON");
        assert_eq!(export_file_name(Some("../x"), "s", "j", FileFormat::Csv, t), "_x.csv");
        assert_eq!(export_file_name(Some("  "), "s", "j", FileFormat::Csv, t), "s_2026-10-08_020509.csv");
    }
}
