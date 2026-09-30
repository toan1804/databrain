//! DataBrain desktop host: wires the Rust backend into Tauri commands/events.

pub mod ai_api;
pub mod api;

use std::sync::Arc;

use api::{AppState, ConnectionView, SaveConnectionArgs, Span};
use databrain_auth::KeychainStore;
use databrain_connector_core::{
    ConnectionConfig, ConnectorInfo, ConnectorKind, DbObject, ObjectDetail, SchemaInfo,
};
use databrain_export::ExportOptions;
use databrain_query_engine::{
    EngineError, EventSink, JobEvent, RunRequest, RunResponse, TestResult,
};
use databrain_result_store::{ColumnStats, FindResult, Page, ResultInfo, ViewSpec};
use databrain_workspace::{
    ConnectionProfile, Folder, FolderKind, HistoryEntry, HistoryQuery, SavedQuery, TabState,
    Workspace,
};
use tauri::{AppHandle, Emitter, Manager, State};

/// Name of the event carrying [`JobEvent`] payloads.
pub const JOB_EVENT: &str = "job-event";

struct TauriUi(AppHandle);

impl ai_api::UiBridge for TauriUi {
    fn emit(&self, channel: &str, payload: serde_json::Value) {
        let _ = self.0.emit(channel, payload);
    }
    fn open_url(&self, url: &str) -> Result<(), String> {
        use tauri_plugin_opener::OpenerExt;
        // Only web URLs; everything else is refused.
        if !(url.starts_with("https://") || url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1")) {
            return Err(format!("refusing to open non-web URL: {url}"));
        }
        self.0.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
    }
}

struct TauriSink(AppHandle);

impl EventSink for TauriSink {
    fn emit(&self, event: JobEvent) {
        let _ = self.0.emit(JOB_EVENT, event);
    }
}

type R<T> = Result<T, EngineError>;

// Every command is `async` so Tauri runs it off the main (UI) thread.

#[tauri::command]
async fn list_connectors(state: State<'_, AppState>) -> R<Vec<ConnectorInfo>> {
    Ok(api::list_connectors(&state))
}

#[tauri::command]
async fn list_connections(state: State<'_, AppState>) -> R<Vec<ConnectionView>> {
    api::list_connections(&state)
}

#[tauri::command]
async fn save_connection(
    state: State<'_, AppState>,
    args: SaveConnectionArgs,
) -> R<ConnectionProfile> {
    api::save_connection(&state, args)
}

#[tauri::command]
async fn delete_connection(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_connection(&state, &id)
}

#[tauri::command]
async fn test_connection(
    state: State<'_, AppState>,
    config: ConnectionConfig,
    secret: Option<String>,
    connection_id: Option<String>,
    ssh_secret: Option<String>,
) -> R<TestResult> {
    api::test_connection(&state, config, secret, connection_id, ssh_secret).await
}

#[tauri::command]
async fn move_to_folder(state: State<'_, AppState>, kind: FolderKind, item_id: String, folder_id: Option<String>) -> R<()> {
    api::move_to_folder(&state, kind, &item_id, folder_id)
}

// ---- notebooks
#[tauri::command]
async fn list_notebooks(state: State<'_, AppState>) -> R<Vec<databrain_workspace::NotebookSummary>> {
    api::list_notebooks(&state)
}
#[tauri::command]
async fn get_notebook(state: State<'_, AppState>, id: String) -> R<databrain_workspace::Notebook> {
    api::get_notebook(&state, &id)
}
#[tauri::command]
async fn save_notebook(state: State<'_, AppState>, notebook: databrain_workspace::Notebook) -> R<databrain_workspace::Notebook> {
    api::save_notebook(&state, notebook)
}
#[tauri::command]
async fn delete_notebook(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_notebook(&state, &id)
}
#[tauri::command]
async fn file_scan_sql(path: String) -> R<String> {
    #[cfg(feature = "duckdb")]
    return api::file_scan_sql(&path);
    #[cfg(not(feature = "duckdb"))]
    Err(databrain_query_engine::EngineError::new("invalid", format!("DuckDB support is not built in ({path})")))
}

// ---- sign-in
#[tauri::command]
async fn sign_in(state: State<'_, AppState>, id: String) -> R<databrain_auth::AuthStatus> {
    ai_api::sign_in(&state, &id).await
}
#[tauri::command]
async fn sign_out(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::sign_out(&state, &id).await
}
#[tauri::command]
async fn auth_status(state: State<'_, AppState>, id: String) -> R<databrain_auth::AuthStatus> {
    ai_api::auth_status(&state, &id)
}
#[tauri::command]
async fn cancel_sign_in(state: State<'_, AppState>) -> R<()> {
    ai_api::cancel_sign_in(&state);
    Ok(())
}

// ---- AI providers
#[tauri::command]
async fn ai_list_providers(state: State<'_, AppState>) -> R<Vec<ai_api::ProviderView>> {
    ai_api::list_providers(&state)
}
#[tauri::command]
async fn ai_save_provider(state: State<'_, AppState>, args: ai_api::SaveProviderArgs) -> R<databrain_workspace::AiProviderRecord> {
    ai_api::save_provider(&state, args)
}
#[tauri::command]
async fn ai_delete_provider(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::delete_provider(&state, &id)
}
#[tauri::command]
async fn ai_list_models(state: State<'_, AppState>, id: String) -> R<Vec<databrain_ai::ModelInfo>> {
    ai_api::list_models(&state, &id).await
}
#[tauri::command]
async fn ai_provider_sign_in(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::provider_sign_in(&state, &id).await
}

// ---- AI agent
#[tauri::command]
async fn ai_send(state: State<'_, AppState>, args: ai_api::AiSendArgs) -> R<String> {
    ai_api::ai_send(&state, args)
}
#[tauri::command]
async fn ai_cancel(state: State<'_, AppState>, run_id: String) -> R<bool> {
    Ok(ai_api::ai_cancel(&state, &run_id))
}
#[tauri::command]
async fn ai_respond(state: State<'_, AppState>, request_id: String, response: serde_json::Value) -> R<bool> {
    Ok(ai_api::ai_respond(&state, &request_id, response))
}
#[tauri::command]
async fn ai_sessions(state: State<'_, AppState>, connection_id: Option<String>) -> R<Vec<databrain_workspace::AiSessionRecord>> {
    ai_api::list_sessions(&state, connection_id)
}
#[tauri::command]
async fn ai_session_messages(state: State<'_, AppState>, session_id: String) -> R<Vec<databrain_workspace::AiMessageRecord>> {
    ai_api::session_messages(&state, &session_id)
}
#[tauri::command]
async fn ai_delete_session(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::delete_session(&state, &id)
}
#[tauri::command]
async fn ai_audit(state: State<'_, AppState>) -> R<Vec<databrain_workspace::AuditEntry>> {
    ai_api::list_audit(&state)
}
#[tauri::command]
async fn mcp_config() -> R<serde_json::Value> {
    Ok(ai_api::mcp_config())
}

// ---- knowledge
#[tauri::command]
async fn kn_get(state: State<'_, AppState>, connection_id: String) -> R<ai_api::KnowledgeView> {
    ai_api::knowledge(&state, &connection_id)
}
#[tauri::command]
async fn kn_index(state: State<'_, AppState>, connection_id: String) -> R<()> {
    ai_api::index_knowledge(&state, &connection_id);
    Ok(())
}
#[tauri::command]
async fn kn_save_note(state: State<'_, AppState>, note: databrain_workspace::KnNote) -> R<databrain_workspace::KnNote> {
    ai_api::save_note(&state, note)
}
#[tauri::command]
async fn kn_delete_note(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::delete_note(&state, &id)
}
#[tauri::command]
async fn kn_clear(state: State<'_, AppState>, connection_id: String) -> R<()> {
    ai_api::clear_knowledge(&state, &connection_id)
}

#[tauri::command]
async fn connect(state: State<'_, AppState>, id: String) -> R<String> {
    api::connect(&state, &id).await
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>, id: String) -> R<()> {
    api::disconnect(&state, &id);
    Ok(())
}

#[tauri::command]
async fn list_schemas(state: State<'_, AppState>, id: String) -> R<Vec<SchemaInfo>> {
    api::list_schemas(&state, &id).await
}

#[tauri::command]
async fn list_objects(state: State<'_, AppState>, id: String, schema: String) -> R<Vec<DbObject>> {
    api::list_objects(&state, &id, &schema).await
}

#[tauri::command]
async fn describe_object(
    state: State<'_, AppState>,
    id: String,
    schema: String,
    name: String,
) -> R<ObjectDetail> {
    api::describe(&state, &id, &schema, &name).await
}

#[tauri::command]
async fn statement_at_cursor(kind: ConnectorKind, text: String, cursor: usize) -> R<Option<Span>> {
    Ok(api::statement_at_cursor(kind, &text, cursor))
}

#[tauri::command]
async fn run_query(state: State<'_, AppState>, req: RunRequest) -> R<RunResponse> {
    api::run_query(&state, req)
}

#[tauri::command]
async fn cancel_query(state: State<'_, AppState>, job_id: String) -> R<bool> {
    Ok(api::cancel_query(&state, &job_id))
}

#[tauri::command]
async fn close_tab(state: State<'_, AppState>, tab_id: String) -> R<()> {
    api::close_tab(&state, &tab_id);
    Ok(())
}

#[tauri::command]
async fn result_info(state: State<'_, AppState>, result_id: String) -> R<ResultInfo> {
    api::result_info(&state, &result_id)
}

#[tauri::command]
async fn fetch_page(
    state: State<'_, AppState>,
    result_id: String,
    view: ViewSpec,
    offset: usize,
    limit: usize,
) -> R<Page> {
    api::fetch_page(&state, result_id, view, offset, limit).await
}

#[tauri::command]
async fn find_in_result(
    state: State<'_, AppState>,
    result_id: String,
    view: ViewSpec,
    query: String,
    limit: usize,
) -> R<FindResult> {
    api::find_in_result(&state, result_id, view, query, limit).await
}

#[tauri::command]
async fn column_stats(
    state: State<'_, AppState>,
    result_id: String,
    view: ViewSpec,
    column: usize,
) -> R<ColumnStats> {
    api::column_stats(&state, result_id, view, column).await
}

#[tauri::command]
async fn export_result(
    state: State<'_, AppState>,
    result_id: String,
    view: ViewSpec,
    options: ExportOptions,
    path: String,
) -> R<u64> {
    api::export_result(&state, result_id, view, options, path).await
}

#[tauri::command]
async fn copy_rows(
    state: State<'_, AppState>,
    result_id: String,
    view: ViewSpec,
    offset: usize,
    limit: usize,
    options: ExportOptions,
) -> R<String> {
    api::copy_rows(&state, result_id, view, offset, limit, options).await
}

#[tauri::command]
async fn release_result(state: State<'_, AppState>, result_id: String) -> R<bool> {
    Ok(api::release_result(&state, &result_id))
}

#[tauri::command]
async fn list_saved_queries(
    state: State<'_, AppState>,
    search: Option<String>,
) -> R<Vec<SavedQuery>> {
    api::list_saved_queries(&state, search)
}

#[tauri::command]
async fn save_query(state: State<'_, AppState>, query: SavedQuery) -> R<SavedQuery> {
    api::save_query(&state, query)
}

#[tauri::command]
async fn delete_saved_query(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_saved_query(&state, &id)
}

#[tauri::command]
async fn list_folders(state: State<'_, AppState>, kind: FolderKind) -> R<Vec<Folder>> {
    api::list_folders(&state, kind)
}

#[tauri::command]
async fn save_folder(state: State<'_, AppState>, folder: Folder) -> R<Folder> {
    api::save_folder(&state, folder)
}

#[tauri::command]
async fn delete_folder(state: State<'_, AppState>, id: String) -> R<()> {
    api::delete_folder(&state, &id)
}

#[tauri::command]
async fn list_history(state: State<'_, AppState>, query: HistoryQuery) -> R<Vec<HistoryEntry>> {
    api::list_history(&state, query)
}

#[tauri::command]
async fn clear_history(state: State<'_, AppState>) -> R<()> {
    api::clear_history(&state)
}

#[tauri::command]
async fn load_tabs(state: State<'_, AppState>) -> R<Vec<TabState>> {
    api::load_tabs(&state)
}

#[tauri::command]
async fn save_tabs(state: State<'_, AppState>, tabs: Vec<TabState>) -> R<()> {
    api::save_tabs(&state, tabs)
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> R<serde_json::Map<String, serde_json::Value>> {
    api::get_settings(&state)
}

#[tauri::command]
async fn set_setting(state: State<'_, AppState>, key: String, value: serde_json::Value) -> R<()> {
    api::set_setting(&state, &key, value)
}

/// macOS menu bar. The default Tauri menu binds ⌘W to "Close Window"; the
/// app uses ⌘W to close editor tabs, so the window item is left out. The Edit
/// menu is required for copy/paste shortcuts in text inputs on macOS.
#[cfg(target_os = "macos")]
fn macos_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, PredefinedMenuItem as P, Submenu};
    let app_menu = Submenu::with_items(
        app,
        "DataBrain",
        true,
        &[
            &P::about(app, None, None)?,
            &P::separator(app)?,
            &P::hide(app, None)?,
            &P::hide_others(app, None)?,
            &P::show_all(app, None)?,
            &P::separator(app)?,
            &P::quit(app, None)?,
        ],
    )?;
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &P::undo(app, None)?,
            &P::redo(app, None)?,
            &P::separator(app)?,
            &P::cut(app, None)?,
            &P::copy(app, None)?,
            &P::paste(app, None)?,
            &P::select_all(app, None)?,
        ],
    )?;
    let window = Submenu::with_items(
        app,
        "Window",
        true,
        &[&P::minimize(app, None)?, &P::maximize(app, None)?, &P::fullscreen(app, None)?],
    )?;
    Menu::with_items(app, &[&app_menu, &edit, &window])
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            let workspace = Arc::new(Workspace::open(dir.join("workspace.db"))?);
            let sink = Arc::new(TauriSink(app.handle().clone()));
            let ui = Arc::new(TauriUi(app.handle().clone()));
            app.manage(AppState::new(workspace, Arc::new(KeychainStore), sink, ui));
            #[cfg(target_os = "macos")]
            app.set_menu(macos_menu(app.handle())?)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_connectors,
            list_connections,
            save_connection,
            delete_connection,
            test_connection,
            connect,
            disconnect,
            list_schemas,
            list_objects,
            describe_object,
            statement_at_cursor,
            run_query,
            cancel_query,
            close_tab,
            result_info,
            fetch_page,
            find_in_result,
            column_stats,
            export_result,
            copy_rows,
            release_result,
            list_saved_queries,
            save_query,
            delete_saved_query,
            list_folders,
            save_folder,
            delete_folder,
            list_history,
            clear_history,
            load_tabs,
            save_tabs,
            get_settings,
            set_setting,
            move_to_folder,
            list_notebooks,
            get_notebook,
            save_notebook,
            delete_notebook,
            file_scan_sql,
            sign_in,
            sign_out,
            auth_status,
            cancel_sign_in,
            ai_list_providers,
            ai_save_provider,
            ai_delete_provider,
            ai_list_models,
            ai_provider_sign_in,
            ai_send,
            ai_cancel,
            ai_respond,
            ai_sessions,
            ai_session_messages,
            ai_delete_session,
            ai_audit,
            mcp_config,
            kn_get,
            kn_index,
            kn_save_note,
            kn_delete_note,
            kn_clear,
        ])
        .run(tauri::generate_context!())
        .expect("error while running DataBrain");
}
