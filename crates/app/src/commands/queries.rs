//! Commands: running SQL, parameters, transactions, plans, SQL files.

use super::*;

#[tauri::command]
pub(crate) fn save_text_file(path: String, contents: String) -> R<()> {
    api::save_text_file(&paths::checked_str(&path, paths::Purpose::SaveText)?, &contents)
}

#[tauri::command]
pub(crate) fn save_base64_file(path: String, data: String) -> R<()> {
    api::save_base64_file(&paths::checked_str(&path, paths::Purpose::SaveImage)?, &data)
}

#[tauri::command]
pub(crate) fn sql_parameters(kind: ConnectorKind, text: String) -> Vec<api::ParamSpanView> {
    api::sql_parameters(kind, &text)
}

#[tauri::command]
pub(crate) fn preview_parameters(
    kind: ConnectorKind,
    values: std::collections::HashMap<String, databrain_connector_core::sql::ParamValue>,
) -> std::collections::HashMap<String, Option<String>> {
    api::preview_parameters(kind, values)
}

#[tauri::command]
pub(crate) async fn explain_plan(
    state: State<'_, AppState>,
    connection_id: String,
    session_key: String,
    sql: String,
    analyze: bool,
    params: Option<std::collections::HashMap<String, databrain_connector_core::sql::ParamValue>>,
) -> R<databrain_query_engine::plan::QueryPlan> {
    api::explain_plan(&state, &connection_id, &session_key, &sql, analyze, &params.unwrap_or_default()).await
}

#[tauri::command]
pub(crate) async fn estimate_cost(
    state: State<'_, AppState>,
    connection_id: String,
    session_key: String,
    sql: String,
    params: Option<std::collections::HashMap<String, databrain_connector_core::sql::ParamValue>>,
) -> R<databrain_connector_core::CostEstimate> {
    api::estimate_cost(&state, &connection_id, &session_key, &sql, &params.unwrap_or_default()).await
}

#[tauri::command]
pub(crate) async fn statement_at_cursor(kind: ConnectorKind, text: String, cursor: usize) -> R<Option<Span>> {
    Ok(api::statement_at_cursor(kind, &text, cursor))
}

/// Every statement of an editor text (next/previous statement keys).
#[tauri::command]
pub(crate) async fn statement_spans(kind: ConnectorKind, text: String) -> R<Vec<Span>> {
    Ok(api::statement_spans(kind, &text))
}

#[tauri::command]
pub(crate) async fn run_query(state: State<'_, AppState>, req: RunRequest) -> R<RunResponse> {
    api::run_query(&state, req)
}

#[tauri::command]
pub(crate) async fn cancel_query(state: State<'_, AppState>, job_id: String) -> R<bool> {
    Ok(api::cancel_query(&state, &job_id))
}

#[tauri::command]
pub(crate) async fn close_tab(state: State<'_, AppState>, tab_id: String) -> R<()> {
    api::close_tab(&state, &tab_id);
    Ok(())
}

#[tauri::command]
pub(crate) async fn read_sql_file(path: String) -> R<sql_files::SqlFile> {
    let path = paths::checked_str(&path, paths::Purpose::Read)?;
    tauri::async_runtime::spawn_blocking(move || sql_files::read_sql_file(&path))
        .await
        .map_err(|e| databrain_query_engine::EngineError::new("internal", e.to_string()))?
}

#[tauri::command]
pub(crate) async fn write_sql_file(path: String, text: String, expected_mtime: Option<i64>) -> R<i64> {
    let path = paths::checked_str(&path, paths::Purpose::WriteSql)?;
    tauri::async_runtime::spawn_blocking(move || sql_files::write_sql_file(&path, &text, expected_mtime))
        .await
        .map_err(|e| databrain_query_engine::EngineError::new("internal", e.to_string()))?
}

/// Modification times of files (null: gone), to notice outside changes.
#[tauri::command]
pub(crate) async fn sql_file_mtimes(paths: Vec<String>) -> R<Vec<Option<i64>>> {
    tauri::async_runtime::spawn_blocking(move || paths.iter().map(|p| sql_files::file_mtime(p)).collect())
        .await
        .map_err(|e| databrain_query_engine::EngineError::new("internal", e.to_string()))
}

#[tauri::command]
pub(crate) fn set_auto_commit(state: State<'_, AppState>, tab_id: String, on: bool) {
    api::set_auto_commit(&state, &tab_id, on);
}

#[tauri::command]
pub(crate) fn transaction_state(state: State<'_, AppState>, connection_id: String, session_key: String) -> bool {
    api::transaction_state(&state, &connection_id, &session_key)
}

#[tauri::command]
pub(crate) async fn end_transaction(state: State<'_, AppState>, connection_id: String, session_key: String, commit: bool) -> R<bool> {
    api::end_transaction(&state, &connection_id, &session_key, commit).await
}
