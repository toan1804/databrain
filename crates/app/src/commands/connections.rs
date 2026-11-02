//! Commands: connection types, saved connections, import and export.

use super::*;

// Every command is `async` so Tauri runs it off the main (UI) thread.

#[tauri::command]
pub(crate) async fn list_connectors(state: State<'_, AppState>) -> R<Vec<ConnectorInfo>> {
    Ok(api::list_connectors(&state))
}

#[tauri::command]
pub(crate) async fn list_connections(state: State<'_, AppState>) -> R<Vec<ConnectionView>> {
    api::list_connections(&state)
}

#[tauri::command]
pub(crate) async fn save_connection(state: State<'_, AppState>, args: SaveConnectionArgs) -> R<ConnectionProfile> {
    api::save_connection(&state, args)
}

#[tauri::command]
pub(crate) async fn delete_connection(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_connection(&state, &id)
}

#[tauri::command]
pub(crate) async fn test_connection(
    state: State<'_, AppState>,
    config: ConnectionConfig,
    secret: Option<String>,
    connection_id: Option<String>,
    ssh_secret: Option<String>,
    test_id: Option<String>,
) -> R<TestResult> {
    api::test_connection(&state, config, secret, connection_id, ssh_secret, test_id).await
}

#[tauri::command]
pub(crate) fn cancel_test_connection(state: State<'_, AppState>, test_id: String) -> bool {
    api::cancel_test_connection(&state, &test_id)
}

#[tauri::command]
pub(crate) async fn move_to_folder(state: State<'_, AppState>, kind: FolderKind, item_id: String, folder_id: Option<String>) -> R<()> {
    api::move_to_folder(&state, kind, &item_id, folder_id)
}

#[tauri::command]
pub(crate) async fn connect(state: State<'_, AppState>, id: String) -> R<String> {
    api::connect(&state, &id).await
}

#[tauri::command]
pub(crate) async fn disconnect(state: State<'_, AppState>, id: String) -> R<()> {
    api::disconnect(&state, &id);
    Ok(())
}

#[tauri::command]
pub(crate) async fn reconnect(state: State<'_, AppState>, id: String) -> R<usize> {
    Ok(api::reconnect(&state, &id))
}

#[tauri::command]
pub(crate) async fn trust_ssh_host_key(state: State<'_, AppState>, host: String, port: u16, fingerprint: String) -> R<usize> {
    api::trust_ssh_host_key(&state, &host, port, &fingerprint)
}

#[tauri::command]
pub(crate) fn export_connections(state: State<'_, AppState>, ids: Option<Vec<String>>, path: String) -> R<usize> {
    conn_io::export_connections(&state, ids, &paths::checked_str(&path, paths::Purpose::WriteJson)?)
}

#[tauri::command]
pub(crate) fn import_connections_preview(state: State<'_, AppState>, source: conn_io::ImportSource, path: Option<String>) -> R<conn_io::ImportPreview> {
    conn_io::import_preview(&state, source, path.map(|p| paths::checked_str(&p, paths::Purpose::Import)).transpose()?)
}

#[tauri::command]
pub(crate) fn import_connections(
    state: State<'_, AppState>,
    source: conn_io::ImportSource,
    path: Option<String>,
    picks: Vec<usize>,
) -> R<conn_io::ImportReport> {
    conn_io::import_apply(&state, source, path.map(|p| paths::checked_str(&p, paths::Purpose::Import)).transpose()?, &picks)
}
