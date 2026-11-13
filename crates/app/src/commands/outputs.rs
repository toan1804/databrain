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
pub(crate) async fn keep_output(state: State<'_, AppState>, handle: String, kept: bool) -> R<api::OutputInfo> {
    api::keep_output(&state, handle, kept).await
}

#[tauri::command]
pub(crate) async fn output_diff_sql(
    state: State<'_, AppState>,
    before: String,
    after: String,
    keys: Vec<String>,
    mapping: Option<Vec<(String, String)>>,
    exact: Option<bool>,
    options: Option<databrain_query_engine::outputs::CompareOptions>,
) -> R<String> {
    api::output_diff_sql(&state, &before, &after, keys, mapping.unwrap_or_default(), exact.unwrap_or(false), &options.unwrap_or_default())
}

#[tauri::command]
pub(crate) async fn output_compare_pairs(
    state: State<'_, AppState>,
    before: String,
    after: String,
    mapping: Option<Vec<(String, String)>>,
    exact: Option<bool>,
    options: Option<databrain_query_engine::outputs::CompareOptions>,
) -> R<Vec<databrain_query_engine::outputs::ComparePair>> {
    api::output_compare_pairs(&state, &before, &after, mapping.unwrap_or_default(), exact.unwrap_or(false), &options.unwrap_or_default())
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

#[tauri::command]
pub(crate) async fn compare_outputs(state: State<'_, AppState>, spec: api::CompareSpec, tab_id: String) -> R<api::CompareRun> {
    api::compare_outputs(&state, spec, &tab_id).await
}

#[tauri::command]
pub(crate) async fn compare_load_source(state: State<'_, AppState>, source: api::CompareSource, tab_id: String, side: String) -> R<api::OutputInfo> {
    let source = match source {
        api::CompareSource::File { path, sheet } => api::CompareSource::File { path: paths::checked_str(&path, paths::Purpose::Read)?, sheet },
        other => other,
    };
    api::compare_load_source(&state, source, &tab_id, &side).await
}

#[tauri::command]
pub(crate) async fn compare_suggest_keys(
    state: State<'_, AppState>,
    before: String,
    after: String,
    mapping: Option<Vec<(String, String)>>,
    exact: Option<bool>,
) -> R<Vec<String>> {
    api::compare_suggest_keys(&state, &before, &after, mapping.unwrap_or_default(), exact.unwrap_or(false)).await
}
