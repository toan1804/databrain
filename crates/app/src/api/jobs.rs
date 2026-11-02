//! Jobs: list, save, run, resume, stop, dry runs and run logs.

use super::*;

// ---------------------------------------------------------------- jobs

impl AppState {
    pub fn job_ctx(&self) -> crate::jobs::JobCtx {
        crate::jobs::JobCtx { engine: self.engine.clone(), workspace: self.workspace.clone(), hub: self.hub.clone(), ui: self.ui.clone(), jobs: self.jobs.clone() }
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
    Ok(JobListing { jobs: state.workspace.list_jobs()?, running: state.jobs.running_ids(), elsewhere: state.workspace.jobs_leased_elsewhere(&state.jobs.owner)? })
}

pub fn get_job(state: &AppState, id: &str) -> Result<databrain_workspace::Job> {
    Ok(state.workspace.get_job(id)?)
}

/// Save a job edited by the user (the AI's tools save through the
/// workspace). A step keeps the AI's mark (`ai_sql`) only while its SQL is
/// as the AI saved it and the user didn't clear it ("Trust this SQL"); a
/// step the user wrote or changed is the user's, and a user save can't mark
/// a step as the AI's.
pub fn save_job(state: &AppState, mut job: databrain_workspace::Job) -> Result<databrain_workspace::Job> {
    let old = if job.id.is_empty() { None } else { state.workspace.get_job(&job.id).ok() };
    for n in &mut job.nodes {
        let kept = old.as_ref().and_then(|o| o.nodes.iter().find(|x| x.id == n.id)).is_some_and(|o| o.ai_sql && o.same_sql(n));
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
    if let Some(o) = state.workspace.job_step_owner(name)? {
        if o.job_id != job_id {
            return Ok(Some(o.describe()));
        }
    }
    // Outputs of job steps don't count: step names decide (a renamed or
    // deleted step's old outputs don't keep its name taken).
    if let Some(o) = state.engine.outputs().resolve(name).filter(|o| o.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name))) {
        if !o.tab_id.starts_with("job:") {
            let from = if o.tab_id.starts_with("nb:") { "a notebook cell" } else { "a query tab" };
            return Ok(Some(format!("output {} (from {from}); rename or remove it first", o.handle)));
        }
    }
    for nb in state.workspace.list_notebooks()? {
        let Ok(nb) = state.workspace.get_notebook(&nb.id) else { continue };
        if nb.cells.iter().any(|c| c.output_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name))) {
            return Ok(Some(format!("a cell of notebook \"{}\"", nb.name)));
        }
    }
    Ok(None)
}

pub fn delete_job(state: &AppState, id: &str) -> Result<()> {
    state.jobs.cancel(id);
    if let Ok(job) = state.workspace.get_job(id) {
        for n in job.nodes {
            state.engine.close_tab(&format!("job:{id}:{}", n.id));
        }
    }
    Ok(state.workspace.delete_job(id)?)
}

/// Run a job now (`nodes`: only these steps).
pub fn run_job(state: &AppState, id: &str, nodes: Option<Vec<String>>) -> Result<databrain_workspace::JobRun> {
    crate::jobs::start(&state.job_ctx(), id, "manual", nodes)
}

/// Run again the steps that did not succeed last time (and what follows them).
pub fn resume_job(state: &AppState, id: &str) -> Result<databrain_workspace::JobRun> {
    crate::jobs::resume(&state.job_ctx(), id)
}

pub fn cancel_job(state: &AppState, id: &str) -> bool {
    state.jobs.cancel(id)
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

pub fn job_runs(state: &AppState, id: &str, limit: Option<i64>) -> Result<Vec<databrain_workspace::JobRun>> {
    Ok(state.workspace.job_runs(id, limit.unwrap_or(30).clamp(1, databrain_workspace::jobs::RUNS_KEPT))?)
}
