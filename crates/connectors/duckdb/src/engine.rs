use databrain_connector_core::{ConnectorError, Result};

use crate::error::map_err;

#[cfg(all(feature = "bundled", feature = "system"))]
compile_error!("enable either the `bundled` or the `system` DuckDB engine, not both (build with --no-default-features)");
#[cfg(not(any(feature = "bundled", feature = "system")))]
compile_error!("enable the `bundled` or the `system` DuckDB engine");

/// Which DuckDB engine this build uses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineStatus {
    /// The engine is compiled into the app.
    pub bundled: bool,
    /// Ready to use (always, when bundled).
    pub available: bool,
    /// The library in use (system engine).
    pub path: Option<String>,
    pub version: Option<String>,
    /// Why no library could be loaded.
    pub error: Option<String>,
    /// Places looked at (system engine, when none was loaded).
    pub tried: Vec<String>,
    /// Library file to look for on this OS.
    pub file_name: String,
}

#[cfg(feature = "system")]
pub(crate) static LOAD_ERROR: std::sync::RwLock<Option<(String, Vec<String>)>> = std::sync::RwLock::new(None);

/// Load the DuckDB library installed on the computer (builds without the
/// bundled engine). `path`: a file or folder chosen by the user. No-op when
/// the engine is bundled or a library is already loaded.
pub fn load_engine(path: Option<&str>) -> EngineStatus {
    #[cfg(feature = "system")]
    if databrain_duckdb_dynload::loaded().is_none() {
        match databrain_duckdb_dynload::load(path.map(std::path::Path::new)) {
            Ok(_) => *LOAD_ERROR.write().unwrap_or_else(|e| e.into_inner()) = None,
            Err(e) => {
                let tried = e.tried.iter().map(|(p, r)| format!("{}: {r}", p.display())).collect();
                *LOAD_ERROR.write().unwrap_or_else(|e| e.into_inner()) = Some((e.to_string(), tried));
            }
        }
    }
    let _ = path;
    engine_status()
}

pub fn engine_status() -> EngineStatus {
    #[cfg(feature = "system")]
    {
        let loaded = databrain_duckdb_dynload::loaded();
        let err = LOAD_ERROR.read().ok().and_then(|g| g.clone());
        EngineStatus {
            bundled: false,
            available: loaded.is_some(),
            path: loaded.map(|l| l.path.to_string_lossy().into_owned()),
            version: loaded.map(|l| l.version.clone()),
            error: if loaded.is_some() { None } else { Some(err.as_ref().map(|e| e.0.clone()).unwrap_or_else(|| "not loaded yet".into())) },
            tried: if loaded.is_some() { vec![] } else { err.map(|e| e.1).unwrap_or_default() },
            file_name: databrain_duckdb_dynload::lib_file_name().into(),
        }
    }
    #[cfg(not(feature = "system"))]
    EngineStatus { bundled: true, available: true, path: None, version: None, error: None, tried: vec![], file_name: String::new() }
}

/// Fails with a guide when this build has no DuckDB engine to use.
pub(crate) fn engine() -> Result<()> {
    if engine_status().available {
        return Ok(());
    }
    Err(ConnectorError::unsupported(format!(
        "DuckDB engine not found. This DataBrain build uses the DuckDB installed on your computer, and {} was not found. \
         Install DuckDB (macOS: brew install duckdb; or download libduckdb for your system from https://duckdb.org/docs/installation), \
         then choose the library in Settings → DuckDB engine, or install the DataBrain download that includes DuckDB.",
        if cfg!(target_os = "macos") {
            "libduckdb.dylib"
        } else if cfg!(windows) {
            "duckdb.dll"
        } else {
            "libduckdb.so"
        }
    )))
}

/// Folder with DuckDB extensions shipped in the app (`<dir>/v1.x.y/<platform>/*.duckdb_extension`).
pub(crate) static EXTENSION_DIR: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Use `dir` as DuckDB's extension directory for new sessions (the app
/// seeds it with the bundled extensions, so nothing is downloaded).
pub fn set_extension_dir(dir: Option<String>) {
    if let Ok(mut g) = EXTENSION_DIR.write() {
        *g = dir;
    }
}

pub fn extension_dir() -> Option<String> {
    EXTENSION_DIR.read().ok().and_then(|g| g.clone())
}

/// DuckDB version and platform of this build, e.g. ("v1.5.6", "osx_arm64").
pub fn version_and_platform() -> Result<(String, String)> {
    engine()?;
    let c = duckdb::Connection::open_in_memory().map_err(map_err)?;
    let v: String = c.query_row("select version()", [], |r| r.get(0)).map_err(map_err)?;
    let p: String = c.query_row("select platform from pragma_platform()", [], |r| r.get(0)).map_err(map_err)?;
    Ok((v, p))
}

/// Load an extension: from the extension directory first (bundled, works
/// offline), downloading it only when it is not there.
pub(crate) fn load_extension(conn: &duckdb::Connection, ext: &str) {
    if conn.execute_batch(&format!("LOAD {ext};")).is_err() {
        let _ = conn.execute_batch(&format!("INSTALL {ext}; LOAD {ext};"));
    }
}

#[cfg(test)]
mod bundled_extensions {
    use super::*;
    use databrain_connector_core::quote_literal;

    /// With the app's extension folder set, Excel loads without any download.
    /// Runs when `crates/app/resources/duckdb-extensions` was fetched
    /// (`node scripts/fetch-duckdb-extensions.mjs`).
    #[test]
    fn loads_bundled_excel_offline() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/resources/duckdb-extensions");
        let (v, p) = version_and_platform().unwrap();
        if !dir.join(&v).join(&p).join("excel.duckdb_extension").is_file() {
            eprintln!("skipped: bundled extensions not fetched");
            return;
        }
        let c = duckdb::Connection::open_in_memory().unwrap();
        c.execute_batch(&format!("SET extension_directory = {}; SET autoinstall_known_extensions = false;", quote_literal(&dir.to_string_lossy()))).unwrap();
        load_extension(&c, "excel");
        let path: String = c.query_row("select install_path from duckdb_extensions() where extension_name = 'excel' and loaded", [], |r| r.get(0)).unwrap();
        // DuckDB writes the path with the system's separators (`\` on Windows).
        let norm = |s: &str| s.replace('\\', "/");
        assert!(norm(&path).starts_with(&norm(&dir.to_string_lossy())), "{path}");
    }
}
