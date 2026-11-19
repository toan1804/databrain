//! Commands: jobs and the background runner.

use super::*;

// ---- notebooks
#[tauri::command]
pub(crate) async fn list_jobs(state: State<'_, AppState>) -> R<api::JobListing> {
    api::list_jobs(&state)
}

#[tauri::command]
pub(crate) async fn table_usage(state: State<'_, AppState>, connection_id: String, schema: String, name: String) -> R<api::TableUsageView> {
    // Reads every job and up to a thousand history entries: off the async runtime.
    let ws = state.workspace.clone();
    api::blocking(move || api::table_usage(&ws, &connection_id, &schema, &name)).await
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
pub(crate) async fn run_moment_preview(
    state: State<'_, AppState>,
    job: databrain_workspace::Job,
    node_id: String,
    date: Option<String>,
) -> R<crate::jobs::RunMomentPreview> {
    api::run_moment_preview(&state, job, &node_id, date.as_deref())
}

/// A desktop notification that a job run ended (the UI sends it when the
/// window isn't focused: the user is in another app).
#[tauri::command]
pub(crate) async fn notify_job_run(title: String, body: String, failed: bool) -> R<()> {
    // Off the async runtime: it starts a process (and waits on Windows).
    tokio::task::spawn_blocking(move || crate::background::notify_as(&title, &body, failed));
    Ok(())
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
pub(crate) async fn job_watermark(state: State<'_, AppState>, id: String, node_id: String, column: String) -> R<Option<databrain_workspace::Watermark>> {
    api::job_watermark(&state, &id, &node_id, &column)
}

#[tauri::command]
pub(crate) async fn reset_job_watermark(state: State<'_, AppState>, id: String, node_id: String) -> R<()> {
    api::reset_job_watermark(&state, &id, &node_id)
}

/// Why a cron expression can't be a job's schedule (what a save would say), or none.
#[tauri::command]
pub(crate) fn check_job_cron(cron: String) -> Option<String> {
    databrain_workspace::jobs::cron_problem(&cron)
}

/// Why a time zone can't be a job's, or none.
#[tauri::command]
pub(crate) fn check_job_time_zone(time_zone: String) -> Option<String> {
    databrain_workspace::jobs::time_zone_problem(&time_zone)
}

/// The IANA time zone names a job may use.
#[tauri::command]
pub(crate) fn job_time_zones() -> Vec<&'static str> {
    databrain_workspace::jobs::time_zone_names()
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
