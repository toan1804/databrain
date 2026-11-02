//! Running SQL: statement spans, parameters, run and cancel, transactions,
//! query plans, saving text to files.

use super::*;

// ------------------------------------------------------------ running queries

#[derive(Debug, Serialize, PartialEq, Eq, ts_rs::TS)]
#[ts(export)]
pub struct Span {
    /// UTF-16 offsets.
    pub start: usize,
    pub end: usize,
    pub sql: String,
}

/// Statement under the cursor (`cursor` is a UTF-16 offset).
pub fn statement_at_cursor(kind: ConnectorKind, text: &str, cursor: usize) -> Option<Span> {
    let spans = split_statements(text, kind);
    let byte = utf16_to_byte(text, cursor);
    statement_at(&spans, byte).map(|s| Span { start: byte_to_utf16(text, s.start), end: byte_to_utf16(text, s.end), sql: s.sql.clone() })
}

/// Every statement of `text` (UTF-16 offsets).
pub fn statement_spans(kind: ConnectorKind, text: &str) -> Vec<Span> {
    split_statements(text, kind).into_iter().map(|s| Span { start: byte_to_utf16(text, s.start), end: byte_to_utf16(text, s.end), sql: s.sql }).collect()
}

/// Write a text file chosen in a save dialog (diagram exports).
pub fn save_text_file(path: &str, contents: &str) -> Result<()> {
    std::fs::write(path, contents).map_err(|e| EngineError::new("io", format!("Could not write {path}: {e}")))
}

/// Write a binary file given as base64 (PNG export).
pub fn save_base64_file(path: &str, data: &str) -> Result<()> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).map_err(|e| EngineError::new("invalid", format!("Bad image data: {e}")))?;
    std::fs::write(path, bytes).map_err(|e| EngineError::new("io", format!("Could not write {path}: {e}")))
}

/// A `:name` parameter in the editor (UTF-16 offsets).
#[derive(Debug, Clone, Serialize)]
pub struct ParamSpanView {
    pub name: String,
    pub from: usize,
    pub to: usize,
    /// Oracle `&name`: replaced by the value as typed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub textual: bool,
}

/// `:name` parameters of an editor text (not in strings/comments, not `::` casts).
pub fn sql_parameters(kind: ConnectorKind, text: &str) -> Vec<ParamSpanView> {
    databrain_connector_core::sql::find_parameters(text, kind)
        .into_iter()
        .map(|p| ParamSpanView { from: byte_to_utf16(text, p.start), to: byte_to_utf16(text, p.end), name: p.name, textual: p.textual })
        .collect()
}

/// The SQL each parameter value becomes (`None` = empty), for the editor's preview.
pub fn preview_parameters(
    kind: ConnectorKind,
    values: std::collections::HashMap<String, databrain_connector_core::sql::ParamValue>,
) -> std::collections::HashMap<String, Option<String>> {
    values.into_iter().map(|(k, v)| (k, databrain_connector_core::sql::render_param(&v, kind))).collect()
}

pub fn run_query(state: &AppState, mut req: RunRequest) -> Result<RunResponse> {
    let text = req.sql.clone();
    // The engine works in bytes; the UI's base offset is UTF-16.
    let base_utf16 = req.base_offset;
    req.base_offset = 0;
    let mut resp = state.engine.run(req)?;
    let statements = match &mut resp {
        RunResponse::Started { statements, .. } => statements,
        RunResponse::NeedsConfirmation { statements, .. } => statements,
    };
    for s in statements.iter_mut() {
        s.start = base_utf16 + byte_to_utf16(&text, s.start);
        s.end = base_utf16 + byte_to_utf16(&text, s.end);
    }
    Ok(resp)
}

pub fn cancel_query(state: &AppState, job_id: &str) -> bool {
    state.engine.cancel(job_id)
}

pub fn close_tab(state: &AppState, tab_id: &str) {
    state.engine.close_tab(tab_id);
    state.engine.set_manual_commit(tab_id, false);
}

/// Manual commit on or off for a tab (its writes then wait for Commit).
pub fn set_auto_commit(state: &AppState, tab_id: &str, on: bool) {
    state.engine.set_manual_commit(tab_id, !on);
}

/// Whether the tab's session on the connection has an open transaction.
pub fn transaction_state(state: &AppState, connection_id: &str, session_key: &str) -> bool {
    state.engine.in_transaction(connection_id, session_key)
}

/// COMMIT or ROLLBACK the tab's transaction; returns whether one is still open.
pub async fn end_transaction(state: &AppState, connection_id: &str, session_key: &str, commit: bool) -> Result<bool> {
    state.engine.end_transaction(connection_id, session_key, commit).await
}

/// The query plan of one statement, on the tab's session (`analyze` runs it: reads only).
pub async fn explain_plan(
    state: &AppState,
    connection_id: &str,
    session_key: &str,
    sql: &str,
    analyze: bool,
    params: &std::collections::HashMap<String, databrain_connector_core::sql::ParamValue>,
) -> Result<databrain_query_engine::plan::QueryPlan> {
    let kind = state.workspace.get_connection(connection_id)?.config.kind;
    let sql = databrain_connector_core::sql::substitute_parameters(sql, kind, params)
        .map_err(|names| invalid(format!("Enter a value for {}", names.iter().map(|n| format!(":{n}")).collect::<Vec<_>>().join(", "))))?;
    state.engine.explain(connection_id, session_key, &sql, analyze).await
}
