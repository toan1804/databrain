//! Commands: column lineage.

use super::*;

#[tauri::command]
pub(crate) async fn sql_lineage(
    state: State<'_, AppState>,
    connection_id: Option<String>,
    kind: ConnectorKind,
    sql: String,
    params: Option<std::collections::HashMap<String, databrain_connector_core::sql::ParamValue>>,
    check: bool,
) -> R<api::LineageView> {
    api::lineage(&state, connection_id.as_deref(), kind, &sql, params.unwrap_or_default(), check).await
}
