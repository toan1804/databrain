use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use super::*;
use crate::{Error, Result, Workspace, new_id, now_ms};

#[derive(Serialize, Deserialize, Default)]
struct Spec {
    #[serde(default)]
    nodes: Vec<JobNode>,
    #[serde(default)]
    edges: Vec<JobEdge>,
    #[serde(default)]
    schedule: JobSchedule,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    params: BTreeMap<String, databrain_connector_core::sql::ParamValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    param_set: Option<String>,
}

impl Spec {
    fn of(job: &Job) -> Self {
        Spec {
            nodes: job.nodes.clone(),
            edges: job.edges.clone(),
            schedule: job.schedule.clone(),
            params: job.params.clone(),
            param_set: job.param_set.clone(),
        }
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
        params: r.get::<_, Option<String>>(8)?.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default(),
    })
}

const RUN_COLS: &str = "id, job_id, trigger, started_at, finished_at, status, nodes_json, error, params_json";

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
                out.push((
                    n.name.to_ascii_lowercase(),
                    StepOwner { job_id: j.id.clone(), job_name: j.name.clone(), node_id: n.id.clone(), step: n.name.clone() },
                ));
            }
        }
        Ok(out)
    }

    /// The step that owns `name` (case-insensitive), if any.
    pub fn job_step_owner(&self, name: &str) -> Result<Option<StepOwner>> {
        let key = name.to_ascii_lowercase();
        Ok(self.job_step_names()?.into_iter().find(|(n, _)| *n == key).map(|(_, o)| o))
    }

    /// Mark every step of `job` as the AI's SQL, keeping the rest of the
    /// job and its edit time (the schedule counts from it).
    pub(crate) fn set_ai_sql_all(&self, job: &Job) -> Result<()> {
        let mut spec = Spec::of(job);
        for n in &mut spec.nodes {
            n.ai_sql = true;
        }
        let spec = serde_json::to_string(&spec).map_err(|e| Error::Invalid(e.to_string()))?;
        self.conn.lock().execute("UPDATE jobs SET spec_json = ?2 WHERE id = ?1", params![job.id, spec])?;
        Ok(())
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
                params: spec.params,
                param_set: spec.param_set,
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
        let spec = serde_json::to_string(&Spec::of(&job)).map_err(|e| Error::Invalid(e.to_string()))?;
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO jobs (id, name, spec_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, spec_json = excluded.spec_json, updated_at = excluded.updated_at",
            params![job.id, job.name.trim(), spec, job.created_at, job.updated_at],
        )?;
        drop(c);
        self.get_job(&job.id)
    }

    /// Drop the logs of runs that are not among each job's latest
    /// [`RUN_LOGS_KEPT`], and of runs older than `older_than_days` (the
    /// history retention; `None` = by count only). Runs keep their summary
    /// (status, steps, rows, errors). Returns how many logs were dropped.
    pub fn prune_job_run_logs(&self, older_than_days: Option<u32>) -> Result<usize> {
        let c = self.conn.lock();
        let mut n = c.execute(
            "UPDATE job_runs SET log_json = NULL WHERE log_json IS NOT NULL AND status <> 'running' AND id NOT IN \
               (SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY job_id ORDER BY id DESC) AS k FROM job_runs) WHERE k <= ?1)",
            params![RUN_LOGS_KEPT],
        )?;
        if let Some(days) = older_than_days {
            let cutoff = now_ms() - days as i64 * 86_400_000;
            n += c.execute("UPDATE job_runs SET log_json = NULL WHERE log_json IS NOT NULL AND status <> 'running' AND started_at < ?1", params![cutoff])?;
        }
        Ok(n)
    }

    pub fn delete_job(&self, id: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM job_runs WHERE job_id = ?1", [id])?;
        c.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Record that the scheduler started the job at `at` (not a user edit).
    /// Put back when the scheduler last started the job (a start that failed).
    pub fn set_job_last_scheduled(&self, id: &str, at: Option<i64>) -> Result<()> {
        self.conn.lock().execute("UPDATE jobs SET last_scheduled_at = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    pub fn mark_job_scheduled(&self, id: &str, at: i64) -> Result<()> {
        self.conn.lock().execute("UPDATE jobs SET last_scheduled_at = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    /// Make a job look saved and last scheduled at `at` (tests of schedules).
    #[doc(hidden)]
    /// Make the job's lease run out now (tests: a computer that slept).
    #[doc(hidden)]
    pub fn expire_job_lease_for_tests(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("UPDATE jobs SET lease_until = 0 WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn backdate_job_for_tests(&self, id: &str, at: i64) -> Result<()> {
        self.conn.lock().execute("UPDATE jobs SET last_scheduled_at = ?2, updated_at = ?2 WHERE id = ?1", params![id, at])?;
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
        let spec = serde_json::to_string(&Spec::of(&job)).map_err(|e| Error::Invalid(e.to_string()))?;
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
        Ok(JobRun {
            id,
            job_id: job_id.into(),
            trigger: trigger.into(),
            started_at: now,
            finished_at: None,
            status: "running".into(),
            nodes: vec![],
            error: None,
            params: Default::default(),
        })
    }

    /// Record the `:name` values a run's steps get.
    pub fn set_job_run_params(&self, run_id: i64, params: &BTreeMap<String, databrain_connector_core::sql::ParamValue>) -> Result<()> {
        let json = (!params.is_empty()).then(|| serde_json::to_string(params).unwrap_or_default());
        self.conn.lock().execute("UPDATE job_runs SET params_json = ?2 WHERE id = ?1", params![run_id, json])?;
        Ok(())
    }

    pub fn update_job_run(&self, run: &JobRun) -> Result<()> {
        use databrain_connector_core::sql::redact_text;
        // Errors can quote the statement (and the password in it).
        let mut nodes = run.nodes.clone();
        for n in &mut nodes {
            n.summary.error = n.summary.error.as_deref().map(redact_text);
        }
        let nodes = serde_json::to_string(&nodes).map_err(|e| Error::Invalid(e.to_string()))?;
        self.conn.lock().execute(
            "UPDATE job_runs SET finished_at = ?2, status = ?3, nodes_json = ?4, error = ?5 WHERE id = ?1",
            params![run.id, run.finished_at, run.status, nodes, run.error.as_deref().map(redact_text)],
        )?;
        Ok(())
    }

    /// Save the whole log of a run (replaces what was saved before).
    pub fn set_job_run_log(&self, run_id: i64, log: &[RunLogEntry]) -> Result<()> {
        // Statements and messages (which can quote one) without passwords and keys.
        let log: Vec<RunLogEntry> = log
            .iter()
            .map(|e| RunLogEntry {
                message: databrain_connector_core::sql::redact_text(&e.message),
                sql: e.sql.as_deref().map(databrain_connector_core::sql::redact_text),
                ..e.clone()
            })
            .collect();
        let json = serde_json::to_string(&log).map_err(|e| Error::Invalid(e.to_string()))?;
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

    /// Runs left "running" by a runner that stopped (app closed or crashed,
    /// background runner killed) are marked cancelled: those whose runner
    /// hasn't reported for [`STALE_RUN_MS`]. Runs of a live runner (another
    /// process) are kept.
    pub fn close_stale_job_runs(&self) -> Result<usize> {
        let before = now_ms() - STALE_RUN_MS;
        let c = self.conn.lock();
        c.execute("UPDATE jobs SET lease_owner = NULL, lease_until = NULL WHERE lease_until IS NOT NULL AND lease_until < ?1", [now_ms()])?;
        Ok(c.execute(
            "UPDATE job_runs SET status = 'cancelled', finished_at = COALESCE(heartbeat_at, started_at), error = 'The app or runner was closed while the job ran' \
             WHERE status = 'running' AND COALESCE(heartbeat_at, started_at) < ?1",
            [before],
        )?)
    }

    /// Take the job for `owner` (a process) until `until` (ms). False when
    /// another owner holds an unexpired lease (it is running the job).
    pub fn claim_job(&self, job_id: &str, owner: &str, until: i64) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE jobs SET lease_owner = ?2, lease_until = ?3 WHERE id = ?1 AND (lease_owner IS NULL OR lease_until IS NULL OR lease_until < ?4 OR lease_owner = ?2)",
            params![job_id, owner, until, now_ms()],
        )?;
        Ok(n == 1)
    }

    /// Give the job back (only `owner`'s lease).
    pub fn release_job(&self, job_id: &str, owner: &str) -> Result<()> {
        self.conn.lock().execute("UPDATE jobs SET lease_owner = NULL, lease_until = NULL WHERE id = ?1 AND lease_owner = ?2", params![job_id, owner])?;
        Ok(())
    }

    /// Who holds the job now (owner, until), if anyone.
    pub fn job_lease(&self, job_id: &str) -> Result<Option<(String, i64)>> {
        let r: Option<(Option<String>, Option<i64>)> =
            self.conn.lock().query_row("SELECT lease_owner, lease_until FROM jobs WHERE id = ?1", [job_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        Ok(match r {
            Some((Some(o), Some(u))) if u >= now_ms() => Some((o, u)),
            _ => None,
        })
    }

    /// Jobs held by another owner right now (running in another process).
    pub fn jobs_leased_elsewhere(&self, owner: &str) -> Result<Vec<String>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare("SELECT id FROM jobs WHERE lease_owner IS NOT NULL AND lease_owner <> ?1 AND lease_until >= ?2")?;
        let ids = stmt.query_map(params![owner, now_ms()], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(ids)
    }

    /// The runner of a run reports it is alive (and who it is).
    pub fn touch_job_run(&self, run_id: i64, runner: &str) -> Result<()> {
        self.conn.lock().execute("UPDATE job_runs SET heartbeat_at = ?2, runner = ?3 WHERE id = ?1", params![run_id, now_ms(), runner])?;
        Ok(())
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
            ai_sql: false,
            last_run: None,
        }
    }

    /// Jobs saved before steps recorded who wrote their SQL: those the
    /// audit log shows the AI created or changed (approved) are marked once,
    /// keeping their edit time; others, and denied proposals, are not.
    /// Logs are kept for each job's latest runs, within the retention;
    /// older runs keep their summary; a running run is never touched.
    #[test]
    fn prunes_run_logs() {
        let ws = Workspace::open_in_memory().unwrap();
        let a = ws.save_job(job(vec![node("a1")], vec![])).unwrap();
        let mut j2 = job(vec![node("b1")], vec![]);
        j2.name = "Other".into();
        let b = ws.save_job(j2).unwrap();
        let entry =
            || RunLogEntry { seq: 1, at: 0, node_id: None, step: None, level: "info".into(), message: "m".into(), sql: None, rows: None, duration_ms: None };
        let mut runs = Vec::new();
        for i in 0..(RUN_LOGS_KEPT + 5) {
            let mut r = ws.start_job_run(&a.id, "manual").unwrap();
            r.status = if i == 0 { "running".into() } else { "success".into() };
            ws.update_job_run(&r).unwrap();
            ws.set_job_run_log(r.id, &[entry()]).unwrap();
            runs.push(r.id);
        }
        let mut other = ws.start_job_run(&b.id, "manual").unwrap();
        other.status = "success".into();
        ws.update_job_run(&other).unwrap();
        ws.set_job_run_log(other.id, &[entry()]).unwrap();
        let has_log = |job: &str, id: i64| !ws.job_run_log(job, id).unwrap().is_empty();

        assert_eq!(ws.prune_job_run_logs(None).unwrap(), 4, "beyond the latest 20, except the one still running");
        assert!(has_log(&a.id, runs[0]), "a running run keeps its log");
        assert!(!has_log(&a.id, runs[1]) && has_log(&a.id, *runs.last().unwrap()));
        assert!(has_log(&b.id, other.id), "counted per job");
        assert_eq!(ws.job_runs(&a.id, 100).unwrap().len(), runs.len(), "summaries stay");
        // By age: older than the retention.
        ws.conn.lock().execute("UPDATE job_runs SET started_at = 0 WHERE id = ?1", [other.id]).unwrap();
        assert_eq!(ws.prune_job_run_logs(Some(30)).unwrap(), 1);
        assert!(!has_log(&b.id, other.id));
    }

    #[test]
    fn marks_jobs_the_ai_wrote_before_steps_recorded_it() {
        let ws = Workspace::open_in_memory().unwrap();
        let mut ai = job(vec![node("a1"), node("a2")], vec![(0, 1)]);
        ai.name = "Nightly compare".into();
        let ai = ws.save_job(ai).unwrap();
        let mut mine = job(vec![node("m1")], vec![]);
        mine.name = "Mine".into();
        let mine = ws.save_job(mine).unwrap();
        let mut denied = job(vec![node("d1")], vec![]);
        denied.name = "Proposed".into();
        let denied = ws.save_job(denied).unwrap();
        let args = |n: &str| serde_json::json!({"name": n, "preview": "…"});
        ws.add_audit(None, None, "create_job", &args(" nightly COMPARE "), "approved", None).unwrap();
        ws.add_audit(None, None, "update_job", &args("Proposed"), "denied", None).unwrap();
        ws.add_audit(None, None, "run_job", &args("Mine"), "approved", None).unwrap();
        ws.conn.lock().execute("DELETE FROM maintenance WHERE name = 'ai_jobs_marked_v1'", []).unwrap();

        assert_eq!(ws.mark_ai_written_jobs().unwrap(), vec!["Nightly compare".to_string()]);
        let after = ws.get_job(&ai.id).unwrap();
        assert!(after.nodes.iter().all(|n| n.ai_sql));
        assert_eq!(after.updated_at, ai.updated_at, "the schedule's base is kept");
        assert_eq!(after.edges, ai.edges);
        assert!(ws.get_job(&mine.id).unwrap().nodes.iter().all(|n| !n.ai_sql), "a run approval isn't authorship");
        assert!(ws.get_job(&denied.id).unwrap().nodes.iter().all(|n| !n.ai_sql));
        // Once.
        let mut cleared = ws.get_job(&ai.id).unwrap();
        cleared.nodes[0].ai_sql = false;
        ws.save_job(cleared).unwrap();
        assert!(ws.mark_ai_written_jobs().unwrap().is_empty());
        assert!(!ws.get_job(&ai.id).unwrap().nodes[0].ai_sql, "a step the user trusted stays trusted");
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
            params: Default::default(),
            param_set: None,
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
        run.nodes = vec![NodeRunRecord {
            node_id: "n1".into(),
            name: "orders".into(),
            summary: NodeRunSummary { status: "success".into(), rows: Some(3), ..Default::default() },
        }];
        ws.update_job_run(&run).unwrap();
        ws.set_job_node_results(&j.id, &run.nodes).unwrap();
        let back = ws.get_job(&j.id).unwrap();
        assert_eq!(back.nodes[1].last_run.as_ref().unwrap().rows, Some(3));
        assert_eq!(back.updated_at, j.updated_at);
        assert_eq!(ws.job_runs(&j.id, 10).unwrap()[0].nodes[0].summary.rows, Some(3));
        assert_eq!(ws.list_jobs().unwrap()[0].last_run.as_ref().unwrap().status, "success");

        // Run log: saved whole, read back per run of its job.
        assert!(ws.job_run_log(&j.id, run.id).unwrap().is_empty());
        let line = RunLogEntry {
            seq: 1,
            at: 5,
            node_id: Some("n1".into()),
            step: Some("orders".into()),
            level: "info".into(),
            message: "Ran".into(),
            sql: Some("select 1".into()),
            rows: Some(1),
            duration_ms: Some(2),
        };
        ws.set_job_run_log(run.id, std::slice::from_ref(&line)).unwrap();
        assert_eq!(ws.job_run_log(&j.id, run.id).unwrap(), vec![line]);
        assert!(ws.job_run_log("other", run.id).is_err());
        assert!(log_sql(&"é".repeat(LOG_SQL_CHARS)).ends_with("more characters)"));

        let live = ws.start_job_run(&j.id, "schedule").unwrap();
        ws.touch_job_run(live.id, "app:1").unwrap();
        assert_eq!(ws.close_stale_job_runs().unwrap(), 0, "its runner reported just now");
        ws.conn.lock().execute("UPDATE job_runs SET heartbeat_at = 1 WHERE id = ?1", [live.id]).unwrap();
        assert_eq!(ws.close_stale_job_runs().unwrap(), 1);

        // Leases: one owner at a time; expired ones can be taken.
        let far = now_ms() + 60_000;
        assert!(ws.claim_job(&j.id, "app:1", far).unwrap());
        assert!(ws.claim_job(&j.id, "app:1", far).unwrap(), "the owner renews");
        assert!(!ws.claim_job(&j.id, "cli:2", far).unwrap());
        assert_eq!(ws.jobs_leased_elsewhere("cli:2").unwrap(), vec![j.id.clone()]);
        assert_eq!(ws.job_lease(&j.id).unwrap().map(|l| l.0).as_deref(), Some("app:1"));
        ws.release_job(&j.id, "cli:2").unwrap();
        assert!(ws.job_lease(&j.id).unwrap().is_some(), "only the owner releases");
        ws.release_job(&j.id, "app:1").unwrap();
        assert!(ws.claim_job(&j.id, "cli:2", 1).unwrap());
        assert!(ws.claim_job(&j.id, "app:1", far).unwrap(), "expired lease");
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
}
