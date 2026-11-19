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
    if let Some(o) = &old
        && o.updated_at != job.updated_at
    {
        return Err(EngineError::new(
            "conflict",
            format!("\"{}\" was changed elsewhere since it was opened (by the assistant or another window): your changes go on top of that version", o.name),
        ));
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
    Ok(state.workspace.save_job(job)?)
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

pub fn cancel_job_step(state: &AppState, id: &str, node_id: &str) -> bool {
    state.jobs.cancel_step(id, node_id)
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
