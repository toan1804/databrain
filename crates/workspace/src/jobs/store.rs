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
    #[serde(default, skip_serializing_if = "RunLimits::is_default")]
    limits: RunLimits,
}

impl Spec {
    fn of(job: &Job) -> Self {
        Spec {
            nodes: job.nodes.clone(),
            edges: job.edges.clone(),
            schedule: job.schedule.clone(),
            params: job.params.clone(),
            param_set: job.param_set.clone(),
            limits: job.limits.clone(),
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
    if let Some(p) = job.schedule.problem() {
        return Err(Error::Invalid(format!("schedule: {p}")));
    }
    Ok(())
}

/// A loop of "run after" between jobs that `job` (with its new
/// `after_jobs`) would close, as job names: `job`, …, `job`.
fn after_loop(job: &Job, others: &[(String, String, Vec<String>)]) -> Option<Vec<String>> {
    let after = |id: &str| -> Vec<String> {
        if id == job.id { job.schedule.after_jobs.clone() } else { others.iter().find(|o| o.0 == id).map(|o| o.2.clone()).unwrap_or_default() }
    };
    let name = |id: &str| if id == job.id { job.name.trim().to_string() } else { others.iter().find(|o| o.0 == id).map(|o| o.1.clone()).unwrap_or_default() };
    // Depth-first from the job through what it runs after, back to it.
    let mut stack = vec![(job.id.clone(), vec![job.id.clone()])];
    let mut seen = std::collections::HashSet::new();
    while let Some((id, path)) = stack.pop() {
        for up in after(&id) {
            if up == job.id {
                // In run order: the job, the one that runs after it, …, the job again.
                let mut names = vec![name(&job.id)];
                names.extend(path[1..].iter().rev().map(|i| name(i)));
                names.push(name(&job.id));
                return Some(names);
            }
            if seen.insert(up.clone()) {
                let mut p = path.clone();
                p.push(up.clone());
                stack.push((up, p));
            }
        }
    }
    None
}

/// Every job but `except`: (id, name, the jobs it runs after), read on `c`
/// (inside a transaction).
fn after_lists(c: &rusqlite::Connection, except: &str) -> Result<Vec<(String, String, Vec<String>)>> {
    let mut stmt = c.prepare("SELECT id, name, spec_json FROM jobs WHERE id <> ?1")?;
    let rows = stmt.query_map([except], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, json) = row?;
        let after = serde_json::from_str::<Spec>(&json).map(|s| s.schedule.after_jobs).unwrap_or_default();
        out.push((id, name, after));
    }
    Ok(out)
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
        ai: r.get::<_, i64>(9)? != 0,
    })
}

const RUN_COLS: &str = "id, job_id, trigger, started_at, finished_at, status, nodes_json, error, params_json, ai";

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
                limits: spec.limits,
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
        let mut now = now_ms();
        if job.id.is_empty() {
            job.id = new_id();
            job.created_at = now;
        } else if let Some(prev) = self.conn.lock().query_row("SELECT updated_at FROM jobs WHERE id = ?1", [&job.id], |r| r.get::<_, i64>(0)).optional()? {
            // Each save is a new version (the editor's saves are refused
            // when based on an older one): two in one millisecond differ too.
            now = now.max(prev + 1);
        }
        // "Run after" other jobs: existing ones, once each, not itself, no loop.
        // Read and written in one write transaction: another process (the
        // background runner, an MCP client) saving the other half of a loop
        // at the same moment waits, then sees this one.
        let mut c = self.conn.lock();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let others = after_lists(&tx, &job.id)?;
        let mut after: Vec<String> = Vec::new();
        for a in std::mem::take(&mut job.schedule.after_jobs) {
            if a != job.id && others.iter().any(|o| o.0 == a) && !after.contains(&a) {
                after.push(a);
            }
        }
        job.schedule.after_jobs = after;
        if let Some(names) = after_loop(&job, &others) {
            return Err(Error::Invalid(format!("jobs would run after each other in a loop: {}", names.join(" → "))));
        }
        job.updated_at = now;
        let spec = serde_json::to_string(&Spec::of(&job)).map_err(|e| Error::Invalid(e.to_string()))?;
        tx.execute(
            "INSERT INTO jobs (id, name, spec_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, spec_json = excluded.spec_json, updated_at = excluded.updated_at",
            params![job.id, job.name.trim(), spec, job.created_at, job.updated_at],
        )?;
        Self::drop_stale_watermarks(&tx, &job)?;
        tx.commit()?;
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

    /// Delete a job, its runs, marks and queued starts; the jobs that ran
    /// after it no longer do (their edit time is kept). One write
    /// transaction: a crash leaves all of it or none, and a save of one of
    /// those jobs at the same moment (another process) isn't overwritten
    /// with its older spec.
    pub fn delete_job(&self, id: &str) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for table in ["job_runs", "job_watermarks", "job_watermarks_pending", "job_after_pending"] {
            tx.execute(&format!("DELETE FROM {table} WHERE job_id = ?1"), [id])?;
        }
        tx.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
        let specs: Vec<(String, String)> = {
            let mut stmt = tx.prepare("SELECT id, spec_json FROM jobs")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (other, json) in specs {
            let Ok(mut spec) = serde_json::from_str::<Spec>(&json) else { continue };
            if !spec.schedule.after_jobs.iter().any(|a| a == id) {
                continue;
            }
            spec.schedule.after_jobs.retain(|a| a != id);
            let json = serde_json::to_string(&spec).map_err(|e| Error::Invalid(e.to_string()))?;
            tx.execute("UPDATE jobs SET spec_json = ?2 WHERE id = ?1", params![other, json])?;
        }
        tx.commit()?;
        Ok(())
    }

    fn job_ids(&self) -> Result<Vec<String>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare("SELECT id FROM jobs")?;
        Ok(stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Jobs (id, name) that run after `id` succeeds.
    pub fn jobs_after(&self, id: &str) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for other in self.job_ids()? {
            if let Ok(j) = self.get_job(&other)
                && j.schedule.after_jobs.iter().any(|a| a == id)
            {
                out.push((j.id, j.name));
            }
        }
        out.sort_by_key(|a| a.1.to_lowercase());
        Ok(out)
    }

    /// An incremental step's mark: the largest value of `column` after its
    /// last successful run. A mark taken from another column (the step's
    /// column changed since) doesn't count.
    pub fn job_watermark(&self, job_id: &str, node_id: &str, column: &str) -> Result<Option<Watermark>> {
        let w = self
            .conn
            .lock()
            .query_row(
                "SELECT column_name, value, value_type, run_id, updated_at FROM job_watermarks WHERE job_id = ?1 AND node_id = ?2",
                params![job_id, node_id],
                |r| Ok(Watermark { column: r.get(0)?, value: r.get(1)?, value_type: r.get(2)?, run_id: r.get(3)?, updated_at: r.get(4)? }),
            )
            .optional()?;
        Ok(w.filter(|w| w.column.eq_ignore_ascii_case(column.trim())))
    }

    /// Keep `value` (of type `value_type`) as the step's mark, taken from
    /// `column` by run `run_id`; a pending mark of the step is done with.
    pub fn set_job_watermark(&self, job_id: &str, node_id: &str, column: &str, value: &str, value_type: Option<&str>, run_id: Option<i64>) -> Result<()> {
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO job_watermarks (job_id, node_id, column_name, value, value_type, run_id, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (job_id, node_id) DO UPDATE SET column_name = excluded.column_name, value = excluded.value, value_type = excluded.value_type,
               run_id = excluded.run_id, updated_at = excluded.updated_at",
            params![job_id, node_id, column.trim(), value, value_type, run_id, now_ms()],
        )?;
        c.execute("DELETE FROM job_watermarks_pending WHERE job_id = ?1 AND node_id = ?2", params![job_id, node_id])?;
        Ok(())
    }

    /// A mark the step took while a step after it didn't succeed: it
    /// becomes the mark when they have (a resume), replaced by the step's
    /// next run. `None` drops it.
    pub fn set_job_watermark_pending(&self, job_id: &str, node_id: &str, pending: Option<&PendingMark>) -> Result<()> {
        let c = self.conn.lock();
        match pending {
            Some(p) => c.execute(
                "INSERT INTO job_watermarks_pending (job_id, node_id, column_name, value, value_type, run_id, taken_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (job_id, node_id) DO UPDATE SET column_name = excluded.column_name, value = excluded.value,
                   value_type = excluded.value_type, run_id = excluded.run_id, taken_at = excluded.taken_at",
                params![job_id, node_id, p.column.trim(), p.value, p.value_type, p.run_id, p.taken_at],
            )?,
            None => c.execute("DELETE FROM job_watermarks_pending WHERE job_id = ?1 AND node_id = ?2", params![job_id, node_id])?,
        };
        Ok(())
    }

    /// The step's pending mark, when taken from `column`.
    pub fn job_watermark_pending(&self, job_id: &str, node_id: &str, column: &str) -> Result<Option<PendingMark>> {
        let p = self
            .conn
            .lock()
            .query_row(
                "SELECT column_name, value, value_type, run_id, taken_at FROM job_watermarks_pending WHERE job_id = ?1 AND node_id = ?2",
                params![job_id, node_id],
                |r| Ok(PendingMark { column: r.get(0)?, value: r.get(1)?, value_type: r.get(2)?, run_id: r.get(3)?, taken_at: r.get(4)? }),
            )
            .optional()?;
        Ok(p.filter(|p| p.column.eq_ignore_ascii_case(column.trim())))
    }

    /// Marks of `job` that its steps no longer have: of a deleted step, or
    /// taken from a column the step doesn't use any more (another column,
    /// or not incremental), so changing the column back starts from the
    /// start value instead of an old mark.
    fn drop_stale_watermarks(c: &rusqlite::Connection, job: &Job) -> Result<()> {
        for table in ["job_watermarks", "job_watermarks_pending"] {
            let rows: Vec<(String, String)> = {
                let mut st = c.prepare(&format!("SELECT node_id, column_name FROM {table} WHERE job_id = ?1"))?;
                st.query_map([&job.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
            };
            for (node_id, column) in rows {
                let current = job.nodes.iter().find(|n| n.id == node_id).and_then(|n| n.incremental_column.as_deref()).map(str::trim).filter(|c| !c.is_empty());
                if current.is_none_or(|cur| !cur.eq_ignore_ascii_case(column.trim())) {
                    c.execute(&format!("DELETE FROM {table} WHERE job_id = ?1 AND node_id = ?2"), params![job.id, node_id])?;
                }
            }
        }
        Ok(())
    }

    /// Forget the step's mark (and a pending one): its next run reads from the start value.
    pub fn reset_job_watermark(&self, job_id: &str, node_id: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM job_watermarks WHERE job_id = ?1 AND node_id = ?2", params![job_id, node_id])?;
        c.execute("DELETE FROM job_watermarks_pending WHERE job_id = ?1 AND node_id = ?2", params![job_id, node_id])?;
        Ok(())
    }

    /// Queue a "run after" start of `job_id` that couldn't happen now (the
    /// job was running): the next scheduler pass, in either process, starts
    /// it. `ai`: under the AI rules ([`JobRun::ai`]). A start of the user's
    /// and one under the AI rules are kept apart (each runs as whose it is,
    /// the user's isn't put under the AI rules); the same kind queued twice
    /// is one start, in its first place in the queue.
    pub fn queue_job_after(&self, job_id: &str, after: &str, ai: bool) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO job_after_pending (job_id, after, added_at, ai) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (job_id, ai) DO UPDATE SET after = excluded.after",
            params![job_id, after, now_ms(), ai as i64],
        )?;
        Ok(())
    }

    /// Jobs with a queued "run after" start: (job id, the job it follows).
    pub fn queued_job_after(&self) -> Result<Vec<(String, String)>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare("SELECT job_id, after FROM job_after_pending ORDER BY added_at, ai")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Take the oldest queued start of `job_id` (one process does): its
    /// trigger and whether it runs under the AI rules, `None` when it isn't
    /// queued (any more).
    pub fn take_job_after(&self, job_id: &str) -> Result<Option<(String, bool)>> {
        let c = self.conn.lock();
        let got: Option<(String, i64)> = c
            .query_row("SELECT after, ai FROM job_after_pending WHERE job_id = ?1 ORDER BY added_at, ai LIMIT 1", [job_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((after, ai)) = got else { return Ok(None) };
        let n = c.execute("DELETE FROM job_after_pending WHERE job_id = ?1 AND after = ?2 AND ai = ?3", params![job_id, after, ai])?;
        Ok((n == 1).then_some((after, ai != 0)))
    }

    /// Record that the scheduler started the job at `at` (not a user edit).
    /// Put back when the scheduler last started the job (a start that failed).
    pub fn set_job_last_scheduled(&self, id: &str, at: Option<i64>) -> Result<()> {
        self.conn.lock().execute("UPDATE jobs SET last_scheduled_at = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    /// Give a scheduled slot back after its start failed: `last_scheduled_at`
    /// goes back to `before` only if it is still `mark` (this process's) and
    /// `owner` still holds the job's lease. False when another runner has
    /// used the slot meanwhile (its mark, or its lease): left as it is.
    pub fn give_back_job_slot(&self, id: &str, owner: &str, mark: i64, before: Option<i64>) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE jobs SET last_scheduled_at = ?4 WHERE id = ?1 AND last_scheduled_at = ?3 AND lease_owner = ?2 AND lease_until >= ?5",
            params![id, owner, mark, before, now_ms()],
        )?;
        Ok(n == 1)
    }

    pub fn mark_job_scheduled(&self, id: &str, at: i64) -> Result<()> {
        self.conn.lock().execute("UPDATE jobs SET last_scheduled_at = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    /// Make the job's lease run out now (tests: a computer that slept).
    #[doc(hidden)]
    pub fn expire_job_lease_for_tests(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("UPDATE jobs SET lease_until = 0 WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Make a run look started (and last heard from) at `at` (tests of the
    /// stale-run check: a runner that died).
    #[doc(hidden)]
    pub fn backdate_job_run_for_tests(&self, run_id: i64, at: i64) -> Result<()> {
        self.conn.lock().execute("UPDATE job_runs SET started_at = ?2, heartbeat_at = ?2 WHERE id = ?1", params![run_id, at])?;
        Ok(())
    }

    /// Make recording a scheduled start fail (`true`) or work again (tests:
    /// a workspace that refuses writes for a while). A trigger of this
    /// connection only, never saved.
    #[doc(hidden)]
    pub fn fail_job_marks_for_tests(&self, fail: bool) -> Result<()> {
        let c = self.conn.lock();
        if fail {
            c.execute_batch(
                "CREATE TEMP TRIGGER IF NOT EXISTS fail_job_marks BEFORE UPDATE OF last_scheduled_at ON jobs BEGIN SELECT RAISE(ABORT, 'database is locked (test)'); END",
            )?;
        } else {
            c.execute_batch("DROP TRIGGER IF EXISTS temp.fail_job_marks")?;
        }
        Ok(())
    }

    /// Make a job look saved and last scheduled at `at` (tests of schedules).
    #[doc(hidden)]
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
            ai: false,
        })
    }

    /// Mark run `run_id` as running every step under the AI rules ([`JobRun::ai`]).
    pub fn set_job_run_ai(&self, run_id: i64) -> Result<()> {
        self.conn.lock().execute("UPDATE job_runs SET ai = 1 WHERE id = ?1", [run_id])?;
        Ok(())
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
    /// One run of a job, by id.
    pub fn job_run(&self, job_id: &str, run_id: i64) -> Result<Option<JobRun>> {
        Ok(self
            .conn
            .lock()
            .query_row(&format!("SELECT {RUN_COLS} FROM job_runs WHERE job_id = ?1 AND id = ?2"), params![job_id, run_id], run_from_row)
            .optional()?)
    }

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
            load_insert_only: false,
            export_folder: None,
            export_file: None,
            export_format: FileFormat::Csv,
            ai_sql: false,
            limits: Default::default(),
            incremental_column: None,
            incremental_start: None,
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
            limits: Default::default(),
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

    /// "Run after" other jobs: unknown ids, the job itself and repeats are
    /// dropped; a loop is refused with its jobs named; deleting a job takes
    /// it out of the lists; a broken schedule is refused.
    #[test]
    fn jobs_run_after_others_without_loops() {
        let ws = Workspace::open_in_memory().unwrap();
        let named = |name: &str, step: &str, after: Vec<String>| {
            let mut j = job(vec![node(step)], vec![]);
            j.name = name.into();
            j.schedule.after_jobs = after;
            j
        };
        let a = ws.save_job(named("Extract", "ex", vec![])).unwrap();
        let b = ws.save_job(named("Transform", "tr", vec![a.id.clone(), a.id.clone(), "gone".into()])).unwrap();
        assert_eq!(b.schedule.after_jobs, vec![a.id.clone()]);
        let mut b2 = b.clone();
        b2.schedule.after_jobs.push(b.id.clone());
        assert_eq!(ws.save_job(b2).unwrap().schedule.after_jobs, vec![a.id.clone()], "not after itself");
        let c = ws.save_job(named("Report", "rp", vec![b.id.clone()])).unwrap();
        assert_eq!(ws.jobs_after(&a.id).unwrap(), vec![(b.id.clone(), "Transform".to_string())]);

        // Extract after Report: Extract → Transform → Report → Extract.
        let mut a2 = a.clone();
        a2.schedule.after_jobs = vec![c.id.clone()];
        let e = ws.save_job(a2).unwrap_err().to_string();
        assert!(e.contains("Extract → Transform → Report → Extract"), "{e}");
        assert!(ws.get_job(&a.id).unwrap().schedule.after_jobs.is_empty(), "not saved");

        // A schedule that can't run is refused (only when it is on).
        let mut bad = ws.get_job(&c.id).unwrap();
        bad.schedule.mode = ScheduleMode::Cron;
        bad.schedule.cron = "61 * * * *".into();
        assert!(ws.save_job(bad.clone()).is_ok(), "off: kept as typed");
        bad.schedule.enabled = true;
        assert!(ws.save_job(bad.clone()).unwrap_err().to_string().contains("minute"));
        bad.schedule.time_zone = "Nowhere/City".into();
        bad.schedule.cron = "0 6 * * *".into();
        assert!(ws.save_job(bad).unwrap_err().to_string().contains("Nowhere/City"));

        let edited = ws.get_job(&c.id).unwrap().updated_at;
        ws.delete_job(&b.id).unwrap();
        let c = ws.get_job(&c.id).unwrap();
        assert!(c.schedule.after_jobs.is_empty());
        assert_eq!(c.updated_at, edited, "its schedule base is kept");
    }

    /// Two processes saving the two halves of a loop at the same moment
    /// (the app and an MCP client): the check and the write are one
    /// transaction, so one is saved and the other refused, never both.
    #[test]
    fn a_loop_saved_from_two_processes_at_once_is_refused() {
        let dir = std::env::temp_dir().join(format!("dbtest-ws-loop-{}", crate::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ws.db");
        let (w1, w2) = (Workspace::open(&path).unwrap(), Workspace::open(&path).unwrap());
        let named = |name: &str, step: &str| {
            let mut j = job(vec![node(step)], vec![]);
            j.name = name.into();
            j
        };
        for round in 0..10 {
            let a = w1.save_job(named(&format!("A{round}"), &format!("a{round}"))).unwrap();
            let b = w1.save_job(named(&format!("B{round}"), &format!("b{round}"))).unwrap();
            let (mut a2, mut b2) = (a.clone(), b.clone());
            a2.schedule.after_jobs = vec![b.id.clone()];
            b2.schedule.after_jobs = vec![a.id.clone()];
            let gate = std::sync::Barrier::new(2);
            let (r1, r2) = std::thread::scope(|s| {
                let t1 = s.spawn(|| {
                    gate.wait();
                    w1.save_job(a2).map(drop)
                });
                let t2 = s.spawn(|| {
                    gate.wait();
                    w2.save_job(b2).map(drop)
                });
                (t1.join().unwrap(), t2.join().unwrap())
            });
            assert_eq!(r1.is_ok() as u8 + r2.is_ok() as u8, 1, "round {round}: {r1:?} {r2:?}");
            let (a, b) = (w1.get_job(&a.id).unwrap(), w1.get_job(&b.id).unwrap());
            assert!(a.schedule.after_jobs.is_empty() || b.schedule.after_jobs.is_empty(), "round {round}: a loop was saved");
        }
        drop((w1, w2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deleting a job takes it out of every list in one go, its marks and
    /// queued starts with it; the other jobs' own settings are kept.
    #[test]
    fn deleting_a_job_cleans_up_everything_of_it() {
        let ws = Workspace::open_in_memory().unwrap();
        let mut a = job(vec![node("da")], vec![]);
        a.name = "Gone".into();
        let a = ws.save_job(a).unwrap();
        let mut b = job(vec![node("db")], vec![]);
        b.name = "Stays".into();
        b.schedule.after_jobs = vec![a.id.clone()];
        b.schedule.time_zone = "Europe/Paris".into();
        let b = ws.save_job(b).unwrap();
        ws.set_job_watermark(&a.id, "n", "id", "5", Some("integer"), Some(1)).unwrap();
        ws.queue_job_after(&a.id, "after:X", false).unwrap();
        ws.queue_job_after(&b.id, "after:Gone", false).unwrap();
        ws.delete_job(&a.id).unwrap();
        assert!(ws.get_job(&a.id).is_err());
        let b = ws.get_job(&b.id).unwrap();
        assert!(b.schedule.after_jobs.is_empty());
        assert_eq!(b.schedule.time_zone, "Europe/Paris");
        assert_eq!(ws.job_watermark(&a.id, "n", "id").unwrap(), None);
        // Its own queued start goes; one it triggered for another job stays (it runs once).
        assert_eq!(ws.queued_job_after().unwrap(), vec![(b.id.clone(), "after:Gone".to_string())]);
    }

    /// Marks belong to a step and its column: saving the job drops those of
    /// a deleted step and of a column the step doesn't use any more, so
    /// switching the column back starts again from the start value.
    #[test]
    fn saving_a_job_drops_marks_its_steps_no_longer_have() {
        let ws = Workspace::open_in_memory().unwrap();
        let mut a = node("inc");
        a.incremental_column = Some("id".into());
        let mut j = ws.save_job(job(vec![a, node("other")], vec![])).unwrap();
        let pending = |v: &str| PendingMark { column: "id".into(), value: v.into(), value_type: Some("integer".into()), run_id: Some(1), taken_at: 1 };
        ws.set_job_watermark(&j.id, "n0", "id", "5", Some("integer"), Some(1)).unwrap();
        ws.set_job_watermark_pending(&j.id, "n0", Some(&pending("7"))).unwrap();
        ws.set_job_watermark(&j.id, "n1", "id", "9", Some("integer"), Some(1)).unwrap();
        // Saved unchanged: kept (the step and its column are the same; case doesn't matter).
        j.nodes[0].incremental_column = Some("ID".into());
        let mut j = ws.save_job(j).unwrap();
        assert_eq!(ws.job_watermark(&j.id, "n0", "id").unwrap().map(|w| w.value).as_deref(), Some("5"));
        assert!(ws.job_watermark_pending(&j.id, "n0", "id").unwrap().is_some());
        // Not incremental (n1): its mark goes.
        assert_eq!(ws.job_watermark(&j.id, "n1", "id").unwrap(), None);
        // Another column: gone, and back to the first one it isn't revived.
        j.nodes[0].incremental_column = Some("updated_at".into());
        let mut j = ws.save_job(j).unwrap();
        j.nodes[0].incremental_column = Some("id".into());
        let mut j = ws.save_job(j).unwrap();
        assert_eq!(ws.job_watermark(&j.id, "n0", "id").unwrap(), None);
        assert_eq!(ws.job_watermark_pending(&j.id, "n0", "id").unwrap(), None);
        // A deleted step's marks go with it.
        ws.set_job_watermark(&j.id, "n0", "id", "5", Some("integer"), Some(1)).unwrap();
        j.nodes.remove(0);
        let j = ws.save_job(j).unwrap();
        assert_eq!(ws.job_watermark(&j.id, "n0", "id").unwrap(), None);
    }
}
