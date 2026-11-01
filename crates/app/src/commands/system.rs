//! Commands: updates, logs, files, terminals and the rest of the app.

use super::*;

/// INSERT batch sizes a connection type allows (load steps).
#[tauri::command]
pub(crate) async fn load_batch_limits(kind: databrain_connector_core::ConnectorKind) -> R<databrain_export::load::BatchLimits> {
    Ok(databrain_export::load::batch_limits(kind))
}

/// Folder export steps use when they have none (Downloads).
#[tauri::command]
pub(crate) async fn default_export_folder() -> R<Option<String>> {
    Ok(dirs::download_dir().or_else(|| dirs::home_dir().map(|h| h.join("Downloads"))).map(|d| d.to_string_lossy().into_owned()))
}

/// Show the file an export step last wrote in Finder/Explorer. Only files
/// recorded by the job are revealed (not arbitrary paths).
#[tauri::command]
pub(crate) async fn reveal_job_file(app: AppHandle, state: State<'_, AppState>, job_id: String, node_id: String) -> R<()> {
    use tauri_plugin_opener::OpenerExt;
    let job = state.workspace.get_job(&job_id)?;
    let file = job
        .nodes
        .iter()
        .find(|n| n.id == node_id)
        .and_then(|n| n.last_run.as_ref())
        .and_then(|r| r.file.clone())
        .ok_or_else(|| EngineError::new("not_found", "This step hasn't written a file yet"))?;
    if !std::path::Path::new(&file).exists() {
        return Err(EngineError::new("not_found", format!("{file} no longer exists")));
    }
    app.opener().reveal_item_in_dir(&file).map_err(|e| EngineError::new("internal", e.to_string()))
}

/// Open a provider's billing page in the browser (https only).
#[tauri::command]
pub(crate) async fn open_link(app: AppHandle, url: String) -> R<()> {
    use tauri_plugin_opener::OpenerExt;
    if !url.starts_with("https://") {
        return Err(EngineError::new("invalid", "only https links can be opened"));
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| EngineError::new("internal", e.to_string()))
}

/// DuckDB engine: bundled, or the system library (found or not).
#[tauri::command]
pub(crate) async fn duckdb_engine_status() -> R<serde_json::Value> {
    #[cfg(feature = "duckdb-engine")]
    return Ok(serde_json::to_value(databrain_connector_duckdb::engine_status()).unwrap_or_default());
    #[cfg(not(feature = "duckdb-engine"))]
    Ok(serde_json::json!({"bundled": false, "available": false, "error": "DuckDB support is not built in", "tried": [], "file_name": ""}))
}

#[tauri::command]
pub(crate) async fn terminal_options() -> R<Vec<databrain_auth::terminal::TerminalOption>> {
    Ok(databrain_auth::terminal::options())
}

/// Thin Oracle driver: installed or not, and what to download by hand.
#[tauri::command]
pub(crate) async fn oracle_agent_info() -> R<serde_json::Value> {
    #[cfg(feature = "oracle")]
    return Ok(serde_json::to_value(databrain_connector_oracle::agent::info()).unwrap_or_default());
    #[cfg(not(feature = "oracle"))]
    Err(databrain_query_engine::EngineError::new("unsupported", "Oracle is not enabled in this build"))
}

/// Install the thin driver from a downloaded file (checked against the built-in SHA-256).
#[tauri::command]
pub(crate) async fn oracle_agent_install_file(path: String) -> R<serde_json::Value> {
    #[cfg(feature = "oracle")]
    {
        let p = std::path::PathBuf::from(path);
        tokio::task::spawn_blocking(move || databrain_connector_oracle::agent::install_from_file(&p))
            .await
            .map_err(|e| databrain_query_engine::EngineError::new("internal", e.to_string()))?
            .map_err(databrain_query_engine::EngineError::from)?;
        Ok(serde_json::to_value(databrain_connector_oracle::agent::info()).unwrap_or_default())
    }
    #[cfg(not(feature = "oracle"))]
    {
        let _ = path;
        Err(databrain_query_engine::EngineError::new("unsupported", "Oracle is not enabled in this build"))
    }
}

/// Open Oracle's Instant Client download page for this platform (fixed URL).
#[tauri::command]
pub(crate) async fn oracle_open_download(state: State<'_, AppState>) -> R<()> {
    #[cfg(feature = "oracle")]
    {
        let url = databrain_connector_oracle::client::platform().download_page;
        state.ui.open_url(url).map_err(|e| databrain_query_engine::EngineError::new("internal", e))
    }
    #[cfg(not(feature = "oracle"))]
    {
        let _ = state;
        Err(databrain_query_engine::EngineError::new("unsupported", "Oracle is not enabled in this build"))
    }
}

#[tauri::command]
pub(crate) async fn credential_store(state: State<'_, AppState>) -> R<api::CredentialStoreView> {
    Ok(api::credential_store(&state))
}

#[tauri::command]
pub(crate) async fn set_credential_store(state: State<'_, AppState>, kind: databrain_auth::StoreKind) -> R<databrain_auth::MigrationReport> {
    api::set_credential_store(&state, kind).await
}

#[tauri::command]
pub(crate) async fn cancel_export(state: State<'_, AppState>, export_id: String) -> R<bool> {
    Ok(state.engine.cancel_export(&export_id))
}

#[tauri::command]
pub(crate) async fn app_in_downloads() -> R<Option<String>> {
    Ok(bundle_in_downloads().map(|p| p.to_string_lossy().into_owned()))
}

/// The latest DataBrain release compared with this app (None: no release found).
#[tauri::command]
pub(crate) async fn reveal_logs(app: AppHandle) -> R<()> {
    use tauri_plugin_opener::OpenerExt;
    let dir = logging::dir(&app.path().app_data_dir().map_err(|e| EngineError::new("internal", e.to_string()))?);
    std::fs::create_dir_all(&dir).map_err(|e| EngineError::new("internal", e.to_string()))?;
    app.opener().reveal_item_in_dir(&dir).map_err(|e| EngineError::new("internal", e.to_string()))
}

#[tauri::command]
pub(crate) async fn check_for_update(app: AppHandle) -> R<Option<updates::UpdateInfo>> {
    let r = updates::check(&app.package_info().version.to_string()).await;
    if let Err(e) = &r {
        tracing::warn!(error = %e.message, "update check failed");
    }
    let mut info = r?;
    if let Some(i) = info.as_mut().filter(|i| i.newer) {
        i.installable = match signed_update(&app).await {
            Ok(u) => u.is_some(),
            Err(e) => {
                tracing::info!(error = %e, "no installable update (download link only)");
                false
            }
        };
    }
    Ok(info)
}

/// The signed update for this build, when the app was built with an updater
/// key and the latest release has one for it.
async fn signed_update(app: &AppHandle) -> Result<Option<tauri_plugin_updater::Update>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let key = app.config().plugins.0.get("updater").and_then(|u| u.get("pubkey")).and_then(|k| k.as_str()).unwrap_or("");
    if key.trim().is_empty() {
        return Err("this build has no updater key".into());
    }
    app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())
}

/// Download, verify and install the latest release, then restart the app.
/// Progress arrives as [`updates::UPDATE_EVENT`]s.
#[tauri::command]
pub(crate) async fn install_update(app: AppHandle) -> R<()> {
    let fail = |e: String| EngineError::new("update", e);
    let update = signed_update(&app).await.map_err(fail)?.ok_or_else(|| fail("No update to install: download it from the release page".into()))?;
    tracing::info!(version = %update.version, "installing update");
    let mut downloaded: u64 = 0;
    let ui = app.clone();
    update
        .download_and_install(
            |chunk, total| {
                downloaded += chunk as u64;
                let _ = ui.emit(updates::UPDATE_EVENT, serde_json::json!({"downloaded": downloaded, "total": total}));
            },
            || {
                let _ = ui.emit(updates::UPDATE_EVENT, serde_json::json!({"finished": true}));
            },
        )
        .await
        .map_err(|e| fail(format!("Could not install the update: {e}")))?;
    app.restart()
}

/// Copy the app to /Applications, open that copy and quit this one. The copy
/// in Downloads is left for the user to delete. `replace`: an existing
/// /Applications/DataBrain.app is replaced (the UI asks first).
#[tauri::command]
pub(crate) async fn move_to_applications(app: AppHandle, replace: bool) -> R<()> {
    let src = bundle_in_downloads().ok_or_else(|| EngineError::new("invalid", "DataBrain is not running from the Downloads folder"))?;
    let name = src.file_name().map(|n| n.to_owned()).unwrap_or_else(|| "DataBrain.app".into());
    let dst = std::path::Path::new("/Applications").join(name);
    if dst.exists() {
        if !replace {
            return Err(EngineError::new("exists", format!("{} already exists", dst.display())));
        }
        std::fs::remove_dir_all(&dst).map_err(|e| EngineError::new("internal", format!("can't replace {}: {e}", dst.display())))?;
    }
    // ditto keeps the bundle's symlinks, permissions and attributes.
    let out = tokio::process::Command::new("/usr/bin/ditto").arg(&src).arg(&dst).output().await.map_err(|e| EngineError::new("internal", e.to_string()))?;
    if !out.status.success() {
        return Err(EngineError::new("internal", format!("copying to Applications failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    std::process::Command::new("/usr/bin/open").arg("-n").arg(&dst).spawn().map_err(|e| EngineError::new("internal", e.to_string()))?;
    app.exit(0);
    Ok(())
}

/// Show the file a finished full export wrote in Finder/Explorer. Only files
/// written by an export of this session are revealed (not arbitrary paths).
#[tauri::command]
pub(crate) async fn reveal_export_file(app: AppHandle, state: State<'_, AppState>, export_id: String) -> R<()> {
    use tauri_plugin_opener::OpenerExt;
    let file = state.engine.exported_file(&export_id).ok_or_else(|| EngineError::new("not_found", "This export hasn't written a file"))?;
    if !file.exists() {
        return Err(EngineError::new("not_found", format!("{} no longer exists", file.display())));
    }
    app.opener().reveal_item_in_dir(&file).map_err(|e| EngineError::new("internal", e.to_string()))
}
