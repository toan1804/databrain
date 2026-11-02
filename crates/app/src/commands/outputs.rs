//! Commands: outputs (`results.<name>`).

use super::*;

// ---- outputs
#[tauri::command]
pub(crate) async fn list_outputs(state: State<'_, AppState>) -> R<Vec<api::OutputInfo>> {
    Ok(api::list_outputs(&state))
}

#[tauri::command]
pub(crate) async fn get_output(state: State<'_, AppState>, reference: String) -> R<api::OutputInfo> {
    api::get_output(&state, &reference)
}

#[tauri::command]
pub(crate) async fn load_output(state: State<'_, AppState>, reference: String) -> R<api::OutputInfo> {
    api::load_output(&state, reference).await
}

#[tauri::command]
pub(crate) async fn rename_output(state: State<'_, AppState>, handle: String, name: Option<String>) -> R<api::OutputInfo> {
    api::rename_output(&state, &handle, name)
}

#[tauri::command]
pub(crate) async fn pin_output(state: State<'_, AppState>, handle: String, pinned: bool) -> R<api::OutputInfo> {
    api::pin_output(&state, handle, pinned).await
}

#[tauri::command]
pub(crate) async fn output_diff_sql(
    state: State<'_, AppState>,
    before: String,
    after: String,
    keys: Vec<String>,
    mapping: Option<Vec<(String, String)>>,
    exact: Option<bool>,
) -> R<String> {
    api::output_diff_sql(&state, &before, &after, keys, mapping.unwrap_or_default(), exact.unwrap_or(false))
}

#[tauri::command]
pub(crate) async fn results_connection(state: State<'_, AppState>) -> R<String> {
    api::results_connection(&state)
}

#[tauri::command]
pub(crate) async fn chart_data(
    state: State<'_, AppState>,
    result_id: String,
    view: ViewSpec,
    spec: databrain_result_store::ChartSpec,
) -> R<databrain_result_store::ChartData> {
    api::chart_data(&state, result_id, view, spec).await
}

#[tauri::command]
pub(crate) async fn drop_output(state: State<'_, AppState>, reference: String) -> R<databrain_query_engine::outputs::OutputInfo> {
    api::drop_output(&state, reference).await
}

#[tauri::command]
pub(crate) async fn drop_unpinned_outputs(state: State<'_, AppState>) -> R<usize> {
    api::drop_unpinned_outputs(&state).await
}
