//! Commands: saved queries, folders, history, notebooks, tabs, settings.

use super::*;

#[tauri::command]
pub(crate) async fn list_notebooks(state: State<'_, AppState>) -> R<Vec<databrain_workspace::NotebookSummary>> {
    api::list_notebooks(&state)
}

#[tauri::command]
pub(crate) async fn get_notebook(state: State<'_, AppState>, id: String) -> R<databrain_workspace::Notebook> {
    api::get_notebook(&state, &id)
}

#[tauri::command]
pub(crate) async fn save_notebook(state: State<'_, AppState>, notebook: databrain_workspace::Notebook) -> R<databrain_workspace::Notebook> {
    api::save_notebook(&state, notebook)
}

#[tauri::command]
pub(crate) async fn delete_notebook(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_notebook(&state, &id)
}

#[tauri::command]
pub(crate) async fn list_saved_queries(
    state: State<'_, AppState>,
    search: Option<String>,
) -> R<Vec<SavedQuery>> {
    api::list_saved_queries(&state, search)
}

#[tauri::command]
pub(crate) async fn save_query(state: State<'_, AppState>, query: SavedQuery) -> R<SavedQuery> {
    api::save_query(&state, query)
}

#[tauri::command]
pub(crate) async fn delete_saved_query(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_saved_query(&state, &id)
}

#[tauri::command]
pub(crate) async fn list_folders(state: State<'_, AppState>, kind: FolderKind) -> R<Vec<Folder>> {
    api::list_folders(&state, kind)
}

#[tauri::command]
pub(crate) async fn save_folder(state: State<'_, AppState>, folder: Folder) -> R<Folder> {
    api::save_folder(&state, folder)
}

#[tauri::command]
pub(crate) async fn delete_folder(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_folder(&state, &id)
}

#[tauri::command]
pub(crate) async fn list_history(state: State<'_, AppState>, query: HistoryQuery) -> R<Vec<HistoryEntry>> {
    api::list_history(&state, query)
}

#[tauri::command]
pub(crate) async fn clear_history(state: State<'_, AppState>, what: Option<databrain_workspace::HistoryClear>) -> R<usize> {
    api::clear_history(&state, what)
}

#[tauri::command]
pub(crate) async fn delete_history_entry(state: State<'_, AppState>, id: i64) -> R<()> {
    api::delete_history_entry(&state, id)
}

#[tauri::command]
pub(crate) async fn load_tabs(state: State<'_, AppState>) -> R<Vec<TabState>> {
    api::load_tabs(&state)
}

#[tauri::command]
pub(crate) async fn save_tabs(state: State<'_, AppState>, tabs: Vec<TabState>) -> R<()> {
    api::save_tabs(&state, tabs)
}

#[tauri::command]
pub(crate) async fn get_settings(state: State<'_, AppState>) -> R<serde_json::Map<String, serde_json::Value>> {
    api::get_settings(&state)
}

#[tauri::command]
pub(crate) async fn set_setting(state: State<'_, AppState>, key: String, value: serde_json::Value) -> R<()> {
    api::set_setting(&state, &key, value)
}
