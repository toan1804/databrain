//! Commands: DuckDB engine and Oracle drivers.

use super::*;

/// Look for the DuckDB library again, or at `path` (file or folder).
#[tauri::command]
pub(crate) async fn duckdb_engine_load(state: State<'_, AppState>, path: Option<String>) -> R<serde_json::Value> {
    #[cfg(feature = "duckdb-engine")]
    return Ok(serde_json::to_value(api::load_duckdb_engine(&state, path)?).unwrap_or_default());
    #[cfg(not(feature = "duckdb-engine"))]
    {
        let _ = (state, path);
        Err(databrain_query_engine::EngineError::new("unsupported", "DuckDB support is not built in"))
    }
}

#[tauri::command]
pub(crate) async fn oracle_client_status(app: tauri::AppHandle, state: State<'_, AppState>, lib_dir: Option<String>) -> R<serde_json::Value> {
    #[cfg(feature = "oracle")]
    {
        let st = api::oracle_client_status(&state, lib_dir, app.path().app_data_dir().ok()).await?;
        Ok(serde_json::to_value(st).unwrap_or_default())
    }
    #[cfg(not(feature = "oracle"))]
    {
        let _ = (app, state, lib_dir);
        Err(databrain_query_engine::EngineError::new("unsupported", "Oracle is not enabled in this build"))
    }
}

#[tauri::command]
pub(crate) async fn oracle_install_client(app: tauri::AppHandle, state: State<'_, AppState>) -> R<serde_json::Value> {
    #[cfg(feature = "oracle")]
    {
        let dir = app.path().app_data_dir().map_err(|e| databrain_query_engine::EngineError::new("internal", e.to_string()))?;
        let st = api::oracle_install_client(&state, dir).await?;
        Ok(serde_json::to_value(st).unwrap_or_default())
    }
    #[cfg(not(feature = "oracle"))]
    {
        let _ = (app, state);
        Err(databrain_query_engine::EngineError::new("unsupported", "Oracle is not enabled in this build"))
    }
}

#[tauri::command]
pub(crate) async fn oracle_cancel_install() -> R<()> {
    #[cfg(feature = "oracle")]
    api::oracle_cancel_install();
    Ok(())
}
