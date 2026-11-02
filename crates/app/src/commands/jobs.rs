//! Commands: jobs and the background runner.

use super::*;

// ---- notebooks
#[tauri::command]
pub(crate) async fn list_jobs(state: State<'_, AppState>) -> R<api::JobListing> {
    api::list_jobs(&state)
}

#[tauri::command]
pub(crate) async fn get_job(state: State<'_, AppState>, id: String) -> R<databrain_workspace::Job> {
    api::get_job(&state, &id)
}

#[tauri::command]
pub(crate) async fn save_job(state: State<'_, AppState>, job: databrain_workspace::Job) -> R<databrain_workspace::Job> {
    api::save_job(&state, job)
}

#[tauri::command]
pub(crate) async fn delete_job(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_job(&state, &id)
}

#[tauri::command]
pub(crate) async fn run_job(
    state: State<'_, AppState>,
    id: String,
    nodes: Option<Vec<String>>,
    params: Option<crate::jobs::RunParams>,
) -> R<databrain_workspace::JobRun> {
    api::run_job_with(&state, &id, nodes, params)
}

#[tauri::command]
pub(crate) async fn list_param_sets(state: State<'_, AppState>) -> R<Vec<databrain_workspace::ParamSet>> {
    api::list_param_sets(&state)
}

#[tauri::command]
pub(crate) async fn save_param_set(state: State<'_, AppState>, set: databrain_workspace::ParamSet) -> R<databrain_workspace::ParamSet> {
    api::save_param_set(&state, set)
}

#[tauri::command]
pub(crate) async fn delete_param_set(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_param_set(&state, &id)
}

#[tauri::command]
pub(crate) async fn resume_job(state: State<'_, AppState>, id: String) -> R<databrain_workspace::JobRun> {
    api::resume_job(&state, &id)
}

#[tauri::command]
pub(crate) async fn cancel_job(state: State<'_, AppState>, id: String) -> R<bool> {
    Ok(api::cancel_job(&state, &id))
}

#[tauri::command]
pub(crate) async fn cancel_job_step(state: State<'_, AppState>, id: String, node_id: String) -> R<bool> {
    Ok(api::cancel_job_step(&state, &id, &node_id))
}

/// Check a load step without keeping any change.
#[tauri::command]
pub(crate) async fn dry_run_job_step(state: State<'_, AppState>, job: databrain_workspace::Job, node_id: String) -> R<databrain_query_engine::LoadDryRun> {
    api::dry_run_job_step(&state, job, &node_id).await
}

/// What else uses an output name (for a step of job `job_id`), or null.
#[tauri::command]
pub(crate) async fn output_name_user(state: State<'_, AppState>, name: String, job_id: String) -> R<Option<String>> {
    api::output_name_user(&state, &name, &job_id)
}

#[tauri::command]
pub(crate) async fn job_run_log(state: State<'_, AppState>, id: String, run_id: i64) -> R<Vec<databrain_workspace::RunLogEntry>> {
    api::job_run_log(&state, &id, run_id)
}

#[tauri::command]
pub(crate) async fn job_runs(state: State<'_, AppState>, id: String, limit: Option<i64>) -> R<Vec<databrain_workspace::JobRun>> {
    api::job_runs(&state, &id, limit)
}

/// Scheduled jobs while the app is closed: on or off, installed, last pass.
#[tauri::command]
pub(crate) async fn background_jobs_status(app: AppHandle, state: State<'_, AppState>) -> R<background::BackgroundStatus> {
    let dir = app.path().app_data_dir().map_err(|e| EngineError::new("internal", e.to_string()))?;
    background::status(&state, &dir)
}

#[tauri::command]
pub(crate) async fn set_background_jobs(app: AppHandle, state: State<'_, AppState>, on: bool) -> R<background::BackgroundStatus> {
    let dir = app.path().app_data_dir().map_err(|e| EngineError::new("internal", e.to_string()))?;
    background::set_enabled(&state, &dir, on)
}
