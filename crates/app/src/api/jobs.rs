//! Jobs: list, save, run, resume, stop, dry runs and run logs.

use super::*;

// ---------------------------------------------------------------- jobs

impl AppState {
    pub fn job_ctx(&self) -> crate::jobs::JobCtx {
        crate::jobs::JobCtx {
            engine: self.engine.clone(),
            workspace: self.workspace.clone(),
            hub: self.hub.clone(),
            ui: self.ui.clone(),
            jobs: self.jobs.clone(),
        }
    }
}

/// Jobs with their next scheduled run and last run, plus which are running.
#[derive(Debug, Clone, Serialize)]
pub struct JobListing {
    pub jobs: Vec<databrain_workspace::JobSummary>,
    pub running: Vec<String>,
    /// Running in another process (the background runner).
    pub elsewhere: Vec<String>,
}

pub fn list_jobs(state: &AppState) -> Result<JobListing> {
    Ok(JobListing {
        jobs: state.workspace.list_jobs()?,
        running: state.jobs.running_ids(),
        elsewhere: state.workspace.jobs_leased_elsewhere(&state.jobs.owner)?,
    })
}

pub fn get_job(state: &AppState, id: &str) -> Result<databrain_workspace::Job> {
    Ok(state.workspace.get_job(id)?)
}

/// Save a job edited by the user (the AI's tools save through the
/// workspace). A step keeps the AI's mark (`ai_sql`) only while its SQL is
/// as the AI saved it and the user didn't clear it ("Trust this SQL"); a
/// step the user wrote or changed is the user's, and a user save can't mark
/// a step as the AI's. A new step that is a copy of one of the AI's
/// ("Duplicate": the same SQL and connections) keeps the mark: a copy of
/// the assistant's SQL is still the assistant's.
///
/// `job.updated_at` is the version the editor's changes are based on: a
/// save made on an older version (the assistant or another window saved the
/// job since) is refused with kind `conflict`. Saved as it is, it would put
/// back what the other change replaced, and SQL the assistant wrote before
/// would come back as the user's (it differs from the assistant's current
/// SQL, so it counts as edited).
pub fn save_job(state: &AppState, mut job: databrain_workspace::Job) -> Result<databrain_workspace::Job> {
    let old = if job.id.is_empty() { None } else { state.workspace.get_job(&job.id).ok() };
    let conflict = |name: &str| {
        EngineError::new(
            "conflict",
            format!("\"{name}\" was changed elsewhere since it was opened (by the assistant or another window): your changes go on top of that version"),
        )
    };
    if let Some(o) = &old
        && o.updated_at != job.updated_at
    {
        return Err(conflict(&o.name));
    }
    for n in &mut job.nodes {
        let kept = match old.as_ref().and_then(|o| o.nodes.iter().find(|x| x.id == n.id)) {
            Some(o) => o.ai_sql && o.same_sql(n),
            None => old.as_ref().is_some_and(|o| o.nodes.iter().any(|x| x.ai_sql && x.same_sql(n))),
        };
        n.ai_sql = n.ai_sql && kept;
    }
    // Names the job doesn't own yet must not be taken by other outputs.
    let owned: Vec<String> = old.as_ref().map(|j| j.nodes.iter().map(|n| n.name.to_ascii_lowercase()).collect()).unwrap_or_default();
    for n in &job.nodes {
        if owned.contains(&n.name.to_ascii_lowercase()) {
            continue;
        }
        if let Some(why) = output_name_user(state, &n.name, &job.id)? {
            return Err(invalid(format!("results.{} is already used by {why}; choose another step name", n.name)));
        }
    }
    // Checked again as it is saved (one write transaction): a save that came
    // between the read above and this one (another window, the assistant,
    // another process) is not overwritten.
    let base = old.as_ref().map(|o| o.updated_at);
    let name = job.name.clone();
    match state.workspace.save_job_based_on(job, base) {
        Err(databrain_workspace::Error::Conflict(_)) => Err(conflict(old.as_ref().map_or(&name, |o| &o.name))),
        Err(databrain_workspace::Error::NotFound(_)) => {
            Err(EngineError::new("not_found", format!("\"{name}\" was deleted since it was opened: nothing was saved")))
        }
        other => Ok(other?),
    }
}

/// What else uses the output name `name` (outside job `job_id`): another
/// job's step, a named output of a query tab or notebook, or a notebook
/// cell's output name. `None` when it's free.
pub fn output_name_user(state: &AppState, name: &str, job_id: &str) -> Result<Option<String>> {
    databrain_ai::jobs::output_name_user(&state.engine, name, job_id).map_err(|e| match e {
        databrain_ai::AiError::Policy(m) => invalid(m),
        other => EngineError::new("internal", other.to_string()),
    })
}

pub fn delete_job(state: &AppState, id: &str) -> Result<()> {
    state.jobs.cancel(id);
    if let Ok(job) = state.workspace.get_job(id) {
        for n in job.nodes {
            state.engine.close_tab(&SessionRole::JobStep { job: id.into(), node: n.id }.key());
        }
    }
    Ok(state.workspace.delete_job(id)?)
}

/// Run a job now (`nodes`: only these steps).
pub fn run_job(state: &AppState, id: &str, nodes: Option<Vec<String>>) -> Result<databrain_workspace::JobRun> {
    run_job_with(state, id, nodes, None)
}

/// [`run_job`] with `:name` values for this run (another set, values over it).
pub fn run_job_with(state: &AppState, id: &str, nodes: Option<Vec<String>>, params: Option<crate::jobs::RunParams>) -> Result<databrain_workspace::JobRun> {
    crate::jobs::start_with(&state.job_ctx(), id, "manual", nodes, params)
}

// ------------------------------------------------------------ parameter sets

pub fn list_param_sets(state: &AppState) -> Result<Vec<databrain_workspace::ParamSet>> {
    Ok(state.workspace.list_param_sets()?)
}

pub fn save_param_set(state: &AppState, set: databrain_workspace::ParamSet) -> Result<databrain_workspace::ParamSet> {
    Ok(state.workspace.save_param_set(set)?)
}

pub fn delete_param_set(state: &AppState, id: &str) -> Result<()> {
    Ok(state.workspace.delete_param_set(id)?)
}

/// Run again the steps that did not succeed last time (and what follows them).
pub fn resume_job(state: &AppState, id: &str) -> Result<databrain_workspace::JobRun> {
    crate::jobs::resume(&state.job_ctx(), id)
}

/// What `:run_date` / `:run_time` are in a step's SQL for a run for `date`.
pub fn run_moment_preview(state: &AppState, job: databrain_workspace::Job, node_id: &str, date: Option<&str>) -> Result<crate::jobs::RunMomentPreview> {
    crate::jobs::run_moment_preview(&state.job_ctx(), &job, node_id, date)
}

/// Stop a job's run: in this process at once; in another (the background
/// runner, `databrain-jobs run`) by a request it reads within seconds.
/// False when it isn't running anywhere.
pub fn cancel_job(state: &AppState, id: &str) -> bool {
    !matches!(crate::jobs::request_stop(&state.job_ctx(), id, "app"), Ok(crate::jobs::StopAsked::NotRunning) | Err(_))
}

/// Stop one step of a job's run: in this process at once; in another (the
/// background runner, `databrain-jobs run`) by a request it reads within
/// seconds. False when the job (or here, the step) isn't running.
pub fn cancel_job_step(state: &AppState, id: &str, node_id: &str) -> bool {
    !matches!(crate::jobs::request_stop_step(&state.job_ctx(), id, node_id, "app"), Ok(crate::jobs::StopAsked::NotRunning) | Err(_))
}

/// Dry run of a load step of `job` as edited (nothing is kept).
pub async fn dry_run_job_step(state: &AppState, job: databrain_workspace::Job, node_id: &str) -> Result<databrain_query_engine::LoadDryRun> {
    crate::jobs::dry_run(&state.job_ctx(), &job, node_id).await
}

/// Log of one run of a job, oldest first.
pub fn job_run_log(state: &AppState, id: &str, run_id: i64) -> Result<Vec<databrain_workspace::RunLogEntry>> {
    Ok(state.workspace.job_run_log(id, run_id)?)
}

/// An incremental step's mark taken from `column` (`None`: none yet, or
/// it was taken from another column).
pub fn job_watermark(state: &AppState, id: &str, node_id: &str, column: &str) -> Result<Option<databrain_workspace::Watermark>> {
    if column.trim().is_empty() {
        return Ok(None);
    }
    Ok(state.workspace.job_watermark(id, node_id, column)?)
}

/// Forget an incremental step's mark: its next run reads from the start value.
pub fn reset_job_watermark(state: &AppState, id: &str, node_id: &str) -> Result<()> {
    // A run going now would set the mark it read again when it ends.
    let elsewhere = state.workspace.job_lease(id).ok().flatten().is_some_and(|(_, until)| until > databrain_workspace::now_ms());
    if state.jobs.is_running(id) || elsewhere {
        return Err(EngineError::new("busy", "The job is running: reset the incremental mark once the run has ended (or stop it first)"));
    }
    Ok(state.workspace.reset_job_watermark(id, node_id)?)
}

pub fn job_runs(state: &AppState, id: &str, limit: Option<i64>) -> Result<Vec<databrain_workspace::JobRun>> {
    Ok(state.workspace.job_runs(id, limit.unwrap_or(30).clamp(1, databrain_workspace::jobs::RUNS_KEPT))?)
}

/// A job step that reads or writes a table (the table explorer's Usage).
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct TableJobStep {
    pub job_id: String,
    pub job_name: String,
    pub step_id: String,
    pub step_name: String,
    /// Its SQL on the table's connection reads the table.
    pub reads: bool,
    /// It loads into the table, or its SQL on the table's connection writes it.
    pub writes: bool,
    /// Named with its schema, so it is this table. False: by its name alone
    /// (or after a `USE`), which is this table only when the session's
    /// default schema is this one.
    pub sure: bool,
}

/// How `t` (a name as written, its parts) names `schema.name` (`schema`
/// may be `catalog.schema`): `Some(true)` for sure (it gives every part, all
/// matching), `Some(false)` with fewer parts (`orders`, or `dbo.orders` for
/// `db.dbo`: the session's default may be another schema or database),
/// `None` not this table. Any case.
fn names_table(t: &[String], schema: &str, name: &str) -> Option<bool> {
    let want: Vec<&str> = schema.split('.').chain([name]).collect();
    if t.is_empty() || t.len() > want.len() {
        return None;
    }
    let tail = &want[want.len() - t.len()..];
    t.iter().zip(tail).all(|(a, b)| a.eq_ignore_ascii_case(b)).then_some(t.len() == want.len())
}

/// A load target as typed (`schema.table`, `"a.b"`, `[a].[b]`, backticks)
/// as name parts: dots inside quotes don't split.
fn target_parts(target: &str) -> Vec<String> {
    let (mut parts, mut cur, mut close) = (Vec::new(), String::new(), None::<char>);
    for ch in target.trim().chars() {
        match (close, ch) {
            (Some(c), _) if ch == c => close = None,
            (Some(_), _) => cur.push(ch),
            (None, '"') => close = Some('"'),
            (None, '`') => close = Some('`'),
            (None, '[') => close = Some(']'),
            (None, '.') => parts.push(std::mem::take(&mut cur)),
            (None, _) => cur.push(ch),
        }
    }
    parts.push(cur);
    parts.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}

/// The job steps that read or write table `schema.name` of connection
/// `connection_id`, from the steps' SQL as written (local: nothing is asked
/// of the server). SQL that can't be read (a procedure call, dynamic SQL)
/// isn't counted. A load step's own SQL runs on DuckDB over `results.*`,
/// so only its before/after SQL and target count.
pub fn table_job_steps(ws: &databrain_workspace::Workspace, connection_id: &str, schema: &str, name: &str) -> Result<Vec<TableJobStep>> {
    let kind = ws.get_connection(connection_id)?.config.kind;
    let mut out = Vec::new();
    for summary in ws.list_jobs()? {
        let Ok(job) = ws.get_job(&summary.id) else { continue };
        for n in &job.nodes {
            let (mut reads, mut writes, mut sure) = (false, false, true);
            let mut scan = |sql: &str| {
                let refs = databrain_connector_core::tables::table_refs(sql, kind);
                for (list, flag) in [(&refs.reads, &mut reads), (&refs.writes, &mut writes)] {
                    for t in list {
                        if let Some(qualified) = names_table(t, schema, name) {
                            *flag = true;
                            sure &= qualified && !refs.changes_default;
                        }
                    }
                }
            };
            if n.kind == databrain_workspace::JobNodeKind::Query && n.connection_id.as_deref() == Some(connection_id) {
                scan(&n.sql);
            }
            if n.kind == databrain_workspace::JobNodeKind::Load && n.target_connection_id.as_deref() == Some(connection_id) {
                for sql in [&n.load_before_sql, &n.load_after_sql].into_iter().flatten() {
                    scan(sql);
                }
                if let Some(q) = n.target_table.as_deref().and_then(|t| names_table(&target_parts(t), schema, name)) {
                    writes = true;
                    sure &= q;
                }
            }
            if reads || writes {
                out.push(TableJobStep {
                    job_id: job.id.clone(),
                    job_name: job.name.clone(),
                    step_id: n.id.clone(),
                    step_name: n.name.clone(),
                    reads,
                    writes,
                    sure,
                });
            }
        }
    }
    Ok(out)
}

/// What the table explorer's Usage shows of a table, all from DataBrain's
/// own records (nothing is asked of the database).
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct TableUsageView {
    /// The learned figures of the user's and jobs' queries on it (when the
    /// connection learns from its history and it was seen).
    pub learned: Option<databrain_workspace::TableUsage>,
    /// The latest entries on its connection whose SQL contains its name
    /// (the UI keeps those naming it as a word).
    pub history: Vec<databrain_workspace::HistoryEntry>,
    pub steps: Vec<TableJobStep>,
}

/// History entries read for Usage.
const USAGE_HISTORY: u32 = 1000;

/// [`TableUsageView`] of `schema.name` on `connection_id`.
pub fn table_usage(ws: &databrain_workspace::Workspace, connection_id: &str, schema: &str, name: &str) -> Result<TableUsageView> {
    // The knowledge base names a table `schema.name` (or `catalog.schema.name`), as the explorer does.
    let learned =
        ws.table_usage(connection_id)?.into_iter().find(|u| names_table(&target_parts(&u.table), schema, name).is_some() && target_parts(&u.table).len() >= 2);
    Ok(TableUsageView {
        learned,
        history: ws.history_mentioning(connection_id, name, USAGE_HISTORY)?,
        steps: table_job_steps(ws, connection_id, schema, name)?,
    })
}

#[cfg(test)]
mod table_name_tests {
    use super::*;

    #[test]
    fn names_and_targets() {
        let p = |s: &str| target_parts(s);
        assert_eq!(p(r#""a.b".c"#), vec!["a.b", "c"]);
        assert_eq!(p("[db].[dbo].[orders]"), vec!["db", "dbo", "orders"]);
        assert_eq!(p("`proj.ds`.t"), vec!["proj.ds", "t"]);
        assert_eq!(p(" sales . orders "), vec!["sales", "orders"]);
        let n = |t: &str, s: &str| names_table(&p(t), s, "orders");
        assert_eq!(n("sales.orders", "sales"), Some(true));
        assert_eq!(n("SALES.Orders", "sales"), Some(true));
        assert_eq!(n("orders", "sales"), Some(false));
        assert_eq!(n("db.dbo.orders", "db.dbo"), Some(true));
        assert_eq!(n("dbo.orders", "db.dbo"), Some(false));
        assert_eq!(n("otherdb.dbo.orders", "db.dbo"), None);
        assert_eq!(n("hr.orders", "sales"), None);
        assert_eq!(n("x.sales.orders", "sales"), None);
        assert_eq!(n("sales.orders_old", "sales"), None);
    }
}
