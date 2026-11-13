//! DataBrain desktop host: wires the Rust backend into Tauri commands/events.

pub mod ai_api;
pub mod api;
pub mod background;
pub mod cli;
mod commands;
pub mod conn_io;
pub mod jobs;
pub mod logging;
pub mod meta_writer;
pub mod paths;
pub mod sql_files;
pub mod updates;

use std::sync::Arc;

use api::{AppState, ConnectionView, SaveConnectionArgs, Span};
use databrain_auth::SwitchableStore;
use databrain_connector_core::{ConnectionConfig, ConnectorInfo, ConnectorKind, DbObject, ObjectDetail, SchemaInfo};
use databrain_export::ExportOptions;
use databrain_query_engine::{EngineError, EventSink, JobEvent, RunRequest, RunResponse, TestResult};
use databrain_result_store::{ColumnStats, FindResult, Page, ResultInfo, ViewSpec};
use databrain_workspace::{ConnectionProfile, Folder, FolderKind, HistoryEntry, HistoryQuery, SavedQuery, TabState, Workspace};
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

/// The running DataBrain.app when it is inside the Downloads folder (macOS):
/// launching it from there makes macOS ask for Downloads access, because the
/// app reads its own bundle. `None` elsewhere.
fn bundle_in_downloads() -> Option<std::path::PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    // …/DataBrain.app/Contents/MacOS/databrain
    let bundle = exe.parent()?.parent()?.parent()?.to_path_buf();
    if bundle.extension().is_none_or(|e| e != "app") {
        return None;
    }
    let downloads = dirs::download_dir().or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))?;
    bundle.starts_with(&downloads).then_some(bundle)
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
    let window = Submenu::with_items(app, "Window", true, &[&P::minimize(app, None)?, &P::maximize(app, None)?, &P::fullscreen(app, None)?])?;
    Menu::with_items(app, &[&app_menu, &edit, &window])
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        // Inactive (no key) unless the build sets plugins.updater.pubkey.
        .plugin(tauri_plugin_updater::Builder::new().target(updates::updater_target()).build())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            logging::init("databrain", Some(&dir), cfg!(debug_assertions));
            let workspace = match Workspace::open(dir.join("workspace.db")) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    // Say why before quitting (a workspace from a newer DataBrain,
                    // a failed backup…), instead of closing without a word.
                    tracing::error!(error = %e, "cannot open the workspace");
                    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.hide();
                    }
                    let handle = app.handle().clone();
                    app.dialog()
                        .message(format!("DataBrain can't open its workspace ({}):\n\n{e}", dir.join("workspace.db").display()))
                        .title("DataBrain")
                        .kind(MessageDialogKind::Error)
                        .show(move |_| handle.exit(1));
                    return Ok(());
                }
            };
            let sink = Arc::new(TauriSink(app.handle().clone()));
            let ui = Arc::new(TauriUi(app.handle().clone()));
            // Passwords/tokens: OS keychain or the local encrypted vault (Settings → Security).
            let store = Arc::new(SwitchableStore::new(api::saved_store_kind(&workspace), &dir));
            let state = AppState::new(workspace, store.clone(), sink, ui);
            state.set_credential_store(store, dir.clone());
            state.move_uri_passwords();
            state.set_snapshot_dir(dir.join("outputs"));
            api::apply_oracle_client_setting(&state);
            api::setup_oracle_agent(&state, &dir);
            // System-engine builds: find the DuckDB library installed on the computer.
            #[cfg(feature = "duckdb-engine")]
            let _ = api::load_duckdb_engine_from_settings(&state.workspace);
            // DuckDB extensions are shipped in the bundle (no runtime download).
            let bundled = app.path().resource_dir().map(|d| d.join("duckdb-extensions")).unwrap_or_default();
            api::seed_duckdb_extensions(&bundled, &dir.join("duckdb_extensions"));
            let jobs = state.job_ctx();
            let (ws, engine) = (state.workspace.clone(), state.engine.clone());
            app.manage(state);
            // History older than the retention setting (default a month), now and daily.
            {
                tauri::async_runtime::spawn(async move {
                    loop {
                        let w = ws.clone();
                        if let Ok(Ok((_, ids))) = tokio::task::spawn_blocking(move || w.prune_history()).await {
                            api::drop_history_results(&engine, &ids);
                        }
                        // AI conversations, with their own retention setting.
                        let w = ws.clone();
                        let _ = tokio::task::spawn_blocking(move || w.prune_ai_history()).await;
                        // Job run logs: each job's latest runs, within the history retention.
                        let w = ws.clone();
                        let _ = tokio::task::spawn_blocking(move || w.prune_job_run_logs(w.history_retention_days())).await;
                        tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
                    }
                });
            }
            // Scheduled jobs run while the app is open.
            tauri::async_runtime::spawn(async move { jobs::spawn_scheduler(jobs) });
            // Idle sessions go back to the server (setting idle_disconnect_minutes).
            {
                let engine = app.state::<AppState>().engine.clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        // Each connection after its own idle time when it has one.
                        engine.close_idle_with(engine.idle_disconnect());
                    }
                });
            }
            // Learn how tables join from query history (every minute).
            {
                let st = app.state::<AppState>();
                let (ws, ui) = (st.workspace.clone(), st.ui.clone());
                tauri::async_runtime::spawn(crate::ai_api::learn_loop(ws, ui));
            }
            #[cfg(target_os = "macos")]
            app.set_menu(macos_menu(app.handle())?)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::connections::list_connectors,
            commands::connections::list_connections,
            commands::connections::save_connection,
            commands::connections::delete_connection,
            commands::connections::test_connection,
            commands::connections::cancel_test_connection,
            commands::connections::connect,
            commands::connections::disconnect,
            commands::connections::reconnect,
            commands::connections::trust_ssh_host_key,
            commands::explorer::list_schemas,
            commands::explorer::list_objects,
            commands::explorer::list_catalogs,
            commands::explorer::list_catalog_schemas,
            commands::explorer::cached_explorer,
            commands::explorer::cached_objects,
            commands::explorer::cached_columns,
            commands::explorer::clear_explorer_cache,
            commands::explorer::refresh_schema,
            commands::explorer::revalidate_explorer,
            commands::explorer::search_objects,
            commands::explorer::complete_tables_local,
            commands::explorer::complete_tables,
            commands::explorer::complete_columns_local,
            commands::explorer::complete_routines_local,
            commands::explorer::complete_routines,
            commands::explorer::package_members,
            commands::explorer::object_ddl,
            commands::explorer::describe_object,
            commands::explorer::schema_tables,
            commands::explorer::table_layout,
            commands::queries::statement_at_cursor,
            commands::queries::statement_spans,
            commands::results::summarize_selection,
            commands::results::load_result,
            commands::results::edit_target,
            commands::results::result_source,
            commands::results::edit_preview,
            commands::results::apply_edits,
            commands::results::result_links,
            commands::explorer::schema_diff,
            commands::queries::explain_plan,
            commands::queries::estimate_cost,
            commands::results::related_rows_sql,
            commands::results::cancel_load,
            commands::results::dry_run_result_load,
            commands::results::check_load_target,
            commands::results::test_data_plan,
            commands::results::cached_test_data_plan,
            commands::results::generate_test_data,
            commands::results::suggest_test_data_patterns,
            commands::queries::sql_parameters,
            commands::lineage::sql_lineage,
            commands::queries::save_text_file,
            commands::queries::save_base64_file,
            commands::queries::preview_parameters,
            commands::queries::run_query,
            commands::queries::cancel_query,
            commands::queries::close_tab,
            commands::queries::set_auto_commit,
            commands::queries::read_sql_file,
            commands::connections::export_connections,
            commands::connections::import_connections_preview,
            commands::connections::import_connections,
            commands::queries::write_sql_file,
            commands::queries::sql_file_mtimes,
            commands::queries::transaction_state,
            commands::queries::end_transaction,
            commands::results::result_info,
            commands::results::fetch_page,
            commands::results::find_in_result,
            commands::results::column_stats,
            commands::results::export_result,
            commands::results::export_full_result,
            commands::system::cancel_export,
            commands::system::reveal_export_file,
            commands::results::copy_rows,
            commands::results::release_result,
            commands::library::list_saved_queries,
            commands::library::save_query,
            commands::library::delete_saved_query,
            commands::library::list_folders,
            commands::library::save_folder,
            commands::library::delete_folder,
            commands::library::list_history,
            commands::library::delete_history_entry,
            commands::library::clear_history,
            commands::library::load_tabs,
            commands::library::save_tabs,
            commands::library::get_settings,
            commands::library::set_setting,
            commands::connections::move_to_folder,
            commands::outputs::list_outputs,
            commands::outputs::get_output,
            commands::outputs::load_output,
            commands::outputs::rename_output,
            commands::outputs::pin_output,
            commands::outputs::keep_output,
            commands::outputs::output_diff_sql,
            commands::outputs::output_compare_pairs,
            commands::outputs::compare_outputs,
            commands::outputs::compare_load_source,
            commands::outputs::compare_suggest_keys,
            commands::outputs::results_connection,
            commands::outputs::chart_data,
            commands::library::list_notebooks,
            commands::jobs::list_jobs,
            commands::jobs::get_job,
            commands::jobs::save_job,
            commands::jobs::delete_job,
            commands::jobs::list_param_sets,
            commands::jobs::save_param_set,
            commands::jobs::delete_param_set,
            commands::jobs::run_job,
            commands::jobs::resume_job,
            commands::system::reveal_logs,
            commands::jobs::cancel_job,
            commands::jobs::cancel_job_step,
            commands::jobs::dry_run_job_step,
            commands::system::load_batch_limits,
            commands::jobs::job_run_log,
            commands::jobs::job_runs,
            commands::system::default_export_folder,
            commands::jobs::output_name_user,
            commands::system::reveal_job_file,
            commands::library::get_notebook,
            commands::library::save_notebook,
            commands::library::delete_notebook,
            commands::explorer::file_scan_sql,
            commands::explorer::excel_sheets,
            commands::ai::sign_in,
            commands::ai::sign_out,
            commands::ai::auth_status,
            commands::ai::cancel_sign_in,
            commands::ai::ai_list_providers,
            commands::ai::ai_save_provider,
            commands::ai::ai_delete_provider,
            commands::ai::ai_list_models,
            commands::ai::ai_provider_sign_in,
            commands::ai::ai_provider_status,
            commands::system::terminal_options,
            commands::system::duckdb_engine_status,
            commands::drivers::duckdb_engine_load,
            commands::ai::ai_provider_usage,
            commands::system::open_link,
            commands::system::app_in_downloads,
            commands::system::check_for_update,
            commands::system::install_update,
            commands::jobs::background_jobs_status,
            commands::jobs::set_background_jobs,
            commands::system::move_to_applications,
            commands::ai::terminal_test,
            commands::ai::ai_provider_sign_out,
            commands::ai::ai_send,
            commands::ai::ai_ghost,
            commands::ai::ai_cancel,
            commands::ai::ai_respond,
            commands::ai::ai_sessions,
            commands::ai::ai_session_messages,
            commands::ai::ai_delete_session,
            commands::ai::ai_audit,
            commands::ai::mcp_config,
            commands::ai::kn_get,
            commands::ai::kn_objects,
            commands::ai::ai_scope,
            commands::ai::ai_put_rule,
            commands::ai::ai_delete_rule,
            commands::ai::kn_index,
            commands::ai::kn_plan,
            commands::system::credential_store,
            commands::system::set_credential_store,
            commands::drivers::oracle_client_status,
            commands::system::oracle_agent_info,
            commands::system::oracle_agent_install_file,
            commands::drivers::oracle_install_client,
            commands::system::oracle_open_download,
            commands::drivers::oracle_cancel_install,
            commands::outputs::drop_output,
            commands::outputs::drop_unpinned_outputs,
            commands::ai::kn_cancel,
            commands::ai::kn_save_note,
            commands::ai::kn_learned,
            commands::ai::kn_set_join_status,
            commands::ai::kn_review_join,
            commands::ai::kn_join_evidence,
            commands::ai::kn_learn_now,
            commands::ai::kn_forget_learned,
            commands::ai::kn_delete_note,
            commands::ai::kn_export_notes,
            commands::ai::kn_check_target,
            commands::ai::hint_layouts,
            commands::ai::kn_read_notes_file,
            commands::ai::kn_import_notes,
            commands::ai::kn_clear,
        ])
        .build(tauri::generate_context!())
        .expect("error while building DataBrain")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Keep each tab's last result for the next start.
                if let Some(state) = app.try_state::<AppState>() {
                    state.engine.outputs().persist_active();
                }
            }
        });
}
