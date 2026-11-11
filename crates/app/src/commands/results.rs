//! Commands: result pages, find, stats, export, editing, loading into tables.

use super::*;

#[tauri::command]
pub(crate) async fn edit_target(state: State<'_, AppState>, result_id: String) -> R<databrain_query_engine::edit::EditTarget> {
    api::edit_target(&state, &result_id).await
}

#[tauri::command]
pub(crate) async fn result_source(state: State<'_, AppState>, result_id: String) -> R<Option<databrain_query_engine::edit::ResultSource>> {
    api::result_source(&state, &result_id)
}

#[tauri::command]
pub(crate) fn edit_preview(
    state: State<'_, AppState>,
    result_id: String,
    target: databrain_query_engine::edit::EditTarget,
    changes: Vec<databrain_query_engine::edit::RowChange>,
) -> R<Vec<databrain_query_engine::edit::EditStatement>> {
    api::edit_preview(&state, &result_id, &target, &changes)
}

#[tauri::command]
pub(crate) async fn apply_edits(
    state: State<'_, AppState>,
    result_id: String,
    changes: Vec<databrain_query_engine::edit::RowChange>,
) -> R<databrain_query_engine::edit::ApplyReport> {
    api::apply_edits(&state, &result_id, changes).await
}

#[tauri::command]
pub(crate) async fn result_links(state: State<'_, AppState>, result_id: String) -> R<Vec<databrain_query_engine::related::ResultLink>> {
    api::result_links(&state, &result_id).await
}

#[tauri::command]
pub(crate) fn related_rows_sql(
    state: State<'_, AppState>,
    result_id: String,
    link: databrain_query_engine::related::ResultLink,
    rows: Vec<Vec<Option<String>>>,
) -> R<String> {
    api::related_rows_sql(&state, &result_id, &link, &rows)
}

#[tauri::command]
pub(crate) async fn load_result(
    state: State<'_, AppState>,
    result_id: String,
    args: api::LoadResultArgs,
    load_id: String,
) -> R<databrain_query_engine::LoadReport> {
    api::load_result(&state, &result_id, args, &load_id).await
}

#[tauri::command]
pub(crate) fn cancel_load(load_id: String) -> bool {
    api::cancel_load(&load_id)
}

#[tauri::command]
pub(crate) async fn dry_run_result_load(state: State<'_, AppState>, result_id: String, args: api::LoadResultArgs) -> R<databrain_query_engine::LoadDryRun> {
    api::dry_run_result_load(&state, &result_id, args).await
}

#[tauri::command]
pub(crate) async fn check_load_target(state: State<'_, AppState>, connection_id: String, table: String) -> R<databrain_query_engine::LoadTarget> {
    api::check_load_target(&state, &connection_id, &table).await
}

#[tauri::command]
pub(crate) async fn test_data_plan(
    state: State<'_, AppState>,
    connection_id: String,
    schema: String,
    name: String,
) -> R<databrain_query_engine::testdata::TestDataPlan> {
    api::test_data_plan(&state, &connection_id, &schema, &name).await
}

#[tauri::command]
pub(crate) async fn generate_test_data(
    state: State<'_, AppState>,
    connection_id: String,
    schema: String,
    name: String,
    spec: databrain_query_engine::testdata::TestDataSpec,
    ai: Option<databrain_ai::testdata::TestDataAi>,
    request_id: Option<String>,
) -> R<api::TestDataOutput> {
    api::generate_test_data(&state, &connection_id, &schema, &name, spec, ai, request_id).await
}

#[tauri::command]
pub(crate) async fn suggest_test_data_patterns(
    state: State<'_, AppState>,
    connection_id: String,
    schema: String,
    name: String,
    spec: databrain_query_engine::testdata::TestDataSpec,
    ai: databrain_ai::testdata::TestDataAi,
    request_id: Option<String>,
) -> R<std::collections::HashMap<String, String>> {
    api::suggest_test_data_patterns(&state, &connection_id, &schema, &name, spec, ai, request_id).await
}

#[tauri::command]
pub(crate) async fn summarize_selection(
    state: State<'_, AppState>,
    result_id: String,
    view: ViewSpec,
    columns: Vec<usize>,
    offset: usize,
    limit: usize,
) -> R<databrain_result_store::SelectionSummary> {
    api::summarize_selection(&state, result_id, view, columns, offset, limit).await
}

#[tauri::command]
pub(crate) async fn result_info(state: State<'_, AppState>, result_id: String) -> R<ResultInfo> {
    api::result_info(&state, &result_id)
}

#[tauri::command]
pub(crate) async fn fetch_page(state: State<'_, AppState>, result_id: String, view: ViewSpec, offset: usize, limit: usize) -> R<Page> {
    api::fetch_page(&state, result_id, view, offset, limit).await
}

#[tauri::command]
pub(crate) async fn find_in_result(
    state: State<'_, AppState>,
    result_id: String,
    view: ViewSpec,
    query: String,
    columns: Option<Vec<usize>>,
    limit: usize,
    whole_word: Option<bool>,
) -> R<FindResult> {
    api::find_in_result(&state, result_id, view, query, columns, limit, whole_word.unwrap_or(false)).await
}

#[tauri::command]
pub(crate) async fn column_stats(state: State<'_, AppState>, result_id: String, view: ViewSpec, column: usize) -> R<ColumnStats> {
    api::column_stats(&state, result_id, view, column).await
}

#[tauri::command]
pub(crate) async fn export_result(state: State<'_, AppState>, result_id: String, view: ViewSpec, options: ExportOptions, path: String) -> R<u64> {
    api::export_result(&state, result_id, view, options, paths::checked_str(&path, paths::Purpose::Export)?).await
}

/// Export every row of a result's query (run again without the row limit).
#[tauri::command]
pub(crate) async fn export_full_result(state: State<'_, AppState>, result_id: String, options: ExportOptions, path: String, export_id: String) -> R<u64> {
    api::export_full_result(&state, &result_id, options, paths::checked_str(&path, paths::Purpose::Export)?, &export_id).await
}

#[tauri::command]
pub(crate) async fn copy_rows(state: State<'_, AppState>, result_id: String, view: ViewSpec, offset: usize, limit: usize, options: ExportOptions) -> R<String> {
    api::copy_rows(&state, result_id, view, offset, limit, options).await
}

#[tauri::command]
pub(crate) async fn release_result(state: State<'_, AppState>, result_id: String) -> R<bool> {
    Ok(api::release_result(&state, &result_id))
}
