use databrain_connector_core::spill_dir::ProcessDir;
use databrain_connector_core::{ConnectorError, Result, quote_literal};

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

/// Where sessions spill (sorts, joins, aggregates bigger than their memory)
/// and how much memory they may use; set by the app (None: DuckDB's own).
/// The spill folder is this process's own under the app's (see
/// [`ProcessDir`]), and each instance spills into a folder under it.
static SPILL_DIR: std::sync::RwLock<Option<ProcessDir>> = std::sync::RwLock::new(None);
static MEMORY_LIMIT: std::sync::RwLock<Option<u64>> = std::sync::RwLock::new(None);

/// Tests that set the process's spill folder take turns.
#[cfg(test)]
pub(crate) static SPILL_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Spill into a folder under `dir` for new sessions. DuckDB's default for an
/// in-memory database is `.tmp` in the current directory, which is `/`
/// (read-only) for an app started from Finder: work bigger than memory
/// failed there. Folders left under `dir` by processes that ended are
/// deleted. When this process can't have a folder of its own there, DuckDB
/// keeps its default.
pub fn set_spill_dir(dir: Option<String>) {
    let claimed = dir.and_then(|d| ProcessDir::claim(std::path::Path::new(&d)).ok());
    if let Ok(mut g) = SPILL_DIR.write() {
        *g = claimed;
    }
}

/// Memory each new DuckDB database may use, in bytes (DuckDB's default is
/// 80% of the computer's, which doesn't leave room for the results budget).
pub fn set_memory_limit(bytes: Option<u64>) {
    if let Ok(mut g) = MEMORY_LIMIT.write() {
        *g = bytes;
    }
}

/// `SET` statements for a new session from [`set_spill_dir`] and
/// [`set_memory_limit`] (empty when neither is set). Run before the
/// sandbox locks the configuration.
pub(crate) fn session_settings() -> String {
    let mut sql = String::new();
    // A folder per instance: DuckDB names its temp files the same in every
    // instance, opens them without an exclusive create and deletes every
    // `duckdb_temp_*` file in the folder when an instance closes, so two
    // instances sharing one overwrite or delete each other's blocks.
    if let Some(dir) = instance_spill_dir("") {
        sql.push_str(&format!("SET temp_directory = {};", quote_literal(&dir)));
    }
    if let Some(b) = MEMORY_LIMIT.read().ok().and_then(|g| *g) {
        sql.push_str(&format!("SET memory_limit = '{}MB';", (b / (1024 * 1024)).max(64)));
    }
    sql
}

/// A spill folder for one sandboxed instance, under this process's
/// ([`set_spill_dir`]; `None` when the app set none): a sealed instance can
/// read and write its `temp_directory`, so it doesn't share the others'.
pub(crate) fn sandbox_spill_dir() -> Option<String> {
    instance_spill_dir("sandbox-")
}

/// `<process folder>/<prefix><n>`, a new `n` each call. DuckDB creates the
/// folder when it first spills.
fn instance_spill_dir(prefix: &str) -> Option<String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let g = SPILL_DIR.read().ok()?;
    let root = g.as_ref()?.path();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(root.join(format!("{prefix}{n}")).to_string_lossy().into_owned())
}

/// This process's spill folder (instances spill into folders under it).
pub fn spill_dir() -> Option<std::path::PathBuf> {
    SPILL_DIR.read().ok().and_then(|g| g.as_ref().map(|d| d.path().to_path_buf()))
}

/// Delete this process's spill folder (the process is quitting); new
/// sessions keep DuckDB's default after this.
pub fn remove_spill_dir() {
    let taken = SPILL_DIR.write().ok().and_then(|mut g| g.take());
    if let Some(d) = taken {
        d.remove();
    }
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
