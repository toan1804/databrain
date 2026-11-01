//! Database drivers set up at start: bundled DuckDB extensions and library,
//! the thin Oracle driver and Oracle Instant Client.

use super::*;

/// Copy the DuckDB extensions shipped in the bundle (`bundled`) into the
/// writable app-data folder `dest` (missing or changed files only) and use it
/// as DuckDB's extension directory. Returns the number of files copied.
/// Signed macOS builds ship them gzipped (`.duckdb_extension.gz`): the notary
/// service refuses executable code not signed with the Developer ID, and
/// re-signing an extension would break DuckDB's own signature on it.
pub fn seed_duckdb_extensions(bundled: &Path, dest: &Path) -> usize {
    /// Size of a gzip member's content (its last 4 bytes, little endian).
    fn gz_size(p: &Path) -> Option<u64> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(p).ok()?;
        f.seek(SeekFrom::End(-4)).ok()?;
        let mut b = [0u8; 4];
        f.read_exact(&mut b).ok()?;
        Some(u32::from_le_bytes(b) as u64)
    }
    fn walk(src: &Path, dst: &Path, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(src) else { return };
        for e in rd.flatten() {
            let s = e.path();
            if s.is_dir() {
                walk(&s, &dst.join(e.file_name()), n);
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let (d, gz) = match name.strip_suffix(".gz") {
                Some(plain) if plain.ends_with(".duckdb_extension") => (dst.join(plain), true),
                _ if name.ends_with(".duckdb_extension") => (dst.join(&name), false),
                _ => continue,
            };
            let want = if gz { gz_size(&s) } else { std::fs::metadata(&s).ok().map(|m| m.len()) };
            if want.is_some() && want == std::fs::metadata(&d).ok().map(|m| m.len()) {
                continue;
            }
            let tmp = d.with_extension("duckdb_extension.tmp");
            let written = std::fs::create_dir_all(dst).is_ok()
                && if gz {
                    std::fs::File::open(&s)
                        .and_then(|f| std::fs::File::create(&tmp).and_then(|mut out| std::io::copy(&mut flate2::read::GzDecoder::new(f), &mut out)))
                        .is_ok()
                } else {
                    std::fs::copy(&s, &tmp).is_ok()
                };
            if written && std::fs::rename(&tmp, &d).is_ok() {
                *n += 1;
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    let mut n = 0;
    walk(bundled, dest, &mut n);
    // The system engine uses DuckDB's own extension folder (~/.duckdb):
    // the extensions DuckDB already installed there match its version.
    #[cfg(feature = "duckdb")]
    databrain_connector_duckdb::set_extension_dir(Some(dest.to_string_lossy().into_owned()));
    n
}

/// Setting: the DuckDB library chosen by the user (system-engine builds).
pub const DUCKDB_LIBRARY_SETTING: &str = "duckdb_library_path";

/// System-engine builds: load the DuckDB library (the saved choice first,
/// then the usual places). Bundled builds: nothing to do.
#[cfg(feature = "duckdb-engine")]
pub fn load_duckdb_engine_from_settings(ws: &Workspace) -> databrain_connector_duckdb::EngineStatus {
    let saved = ws.get_setting(DUCKDB_LIBRARY_SETTING).ok().flatten().and_then(|v| v.as_str().map(str::to_string)).filter(|s| !s.trim().is_empty());
    databrain_connector_duckdb::load_engine(saved.as_deref())
}

/// Load the library at `path` (a file or folder) and remember it when it works.
#[cfg(feature = "duckdb-engine")]
pub fn load_duckdb_engine(state: &AppState, path: Option<String>) -> Result<databrain_connector_duckdb::EngineStatus> {
    let path = path.filter(|p| !p.trim().is_empty());
    let st = databrain_connector_duckdb::load_engine(path.as_deref());
    if let (Some(p), true, false) = (&path, st.available, st.bundled) {
        let from_it = st.path.as_deref().is_some_and(|l| std::path::Path::new(l).starts_with(p));
        // Loaded from it, or another library is already in use (a process
        // can't switch): remember it for the next start.
        if from_it || std::path::Path::new(p).exists() {
            state.workspace.set_setting(DUCKDB_LIBRARY_SETTING, &serde_json::json!(if from_it { st.path.clone().unwrap_or_default() } else { p.clone() }))?;
        }
    }
    Ok(st)
}

/// Setting: where the thin Oracle driver is downloaded from (empty = default release).
pub const ORACLE_AGENT_URL_SETTING: &str = "oracle_agent_url";

/// UI event channel for the thin driver's first-use download.
pub const ORACLE_AGENT_EVENT: &str = "oracle-agent";

/// Thin Oracle driver: download folder (`<app data>/oracle-agent`), release
/// location, and download progress for the UI.
pub fn setup_oracle_agent(state: &AppState, app_dir: &Path) {
    #[cfg(feature = "oracle")]
    {
        use databrain_connector_oracle::agent;
        agent::set_dir(Some(app_dir.join("oracle-agent")));
        agent::set_base_url(state.workspace.get_setting(ORACLE_AGENT_URL_SETTING).ok().flatten().and_then(|v| v.as_str().map(str::to_string)));
        let ui = state.ui.clone();
        agent::set_progress_hook(Some(Arc::new(move |p: agent::Download| {
            ui.emit(
                ORACLE_AGENT_EVENT,
                match p {
                    agent::Download::Progress { done, total } => serde_json::json!({"type": "progress", "done": done, "total": total}),
                    agent::Download::Finished { ok } => serde_json::json!({"type": "finished", "ok": ok}),
                },
            )
        })));
    }
    #[cfg(not(feature = "oracle"))]
    let _ = (state, app_dir);
}

/// Setting with the Instant Client folder found or installed by DataBrain.
pub const ORACLE_CLIENT_SETTING: &str = "oracle_client_dir";

/// Setting: SHA-256 of the libraries in that folder when DataBrain installed
/// or adopted it (`{"dir": …, "files": {name: sha256}}`). They are loaded
/// into the app, so they are checked again before Instant Client is loaded,
/// like the thin driver's helper is before it runs.
pub const ORACLE_CLIENT_MANIFEST_SETTING: &str = "oracle_client_manifest";

#[cfg(feature = "oracle")]
#[derive(serde::Serialize, serde::Deserialize)]
struct ClientManifest {
    dir: String,
    files: databrain_connector_oracle::client::Manifest,
}

/// Hash the libraries of `dir`, remember them, and check them before loading.
#[cfg(feature = "oracle")]
fn trust_oracle_client(ws: &Workspace, dir: &str) -> Result<()> {
    use databrain_connector_oracle::client;
    let files = client::manifest(Path::new(dir)).map_err(|e| EngineError::new("install", e))?;
    ws.set_setting(ORACLE_CLIENT_MANIFEST_SETTING, &serde_json::to_value(ClientManifest { dir: dir.to_string(), files: files.clone() }).unwrap_or_default())?;
    client::set_trusted(Some((dir.to_string(), files)));
    Ok(())
}

/// Use the saved Instant Client folder for Oracle connections without one,
/// and check its libraries against the saved manifest before they load. A
/// folder saved before manifests were kept is hashed now, in the background
/// (once; from then on it is checked).
pub fn apply_oracle_client_setting(state: &AppState) {
    #[cfg(feature = "oracle")]
    {
        use databrain_connector_oracle::client;
        let dir = state.workspace.get_setting(ORACLE_CLIENT_SETTING).ok().flatten().and_then(|v| v.as_str().map(str::to_string)).filter(|d| !d.trim().is_empty());
        let saved: Option<ClientManifest> = state.workspace.get_setting(ORACLE_CLIENT_MANIFEST_SETTING).ok().flatten().and_then(|v| serde_json::from_value(v).ok());
        client::set_default_dir(dir.clone());
        match (dir, saved) {
            (Some(d), Some(m)) if m.dir == d => client::set_trusted(Some((d, m.files))),
            (Some(d), _) => {
                client::set_trusted(None);
                let ws = state.workspace.clone();
                std::thread::spawn(move || {
                    if let Err(e) = trust_oracle_client(&ws, &d) {
                        tracing::warn!(dir = %d, error = %e.message, "Instant Client folder not hashed");
                    }
                });
            }
            (None, _) => client::set_trusted(None),
        }
    }
    #[cfg(not(feature = "oracle"))]
    let _ = state;
}

/// Is Oracle Instant Client available? (`lib_dir`: a folder to check first.)
#[cfg(feature = "oracle")]
pub async fn oracle_client_status(state: &AppState, lib_dir: Option<String>, app_dir: Option<PathBuf>) -> Result<databrain_connector_oracle::client::ClientStatus> {
    let extra: Vec<PathBuf> = app_dir.into_iter().map(|d| d.join("oracle")).collect();
    let st = blocking(move || Ok(databrain_connector_oracle::client::status(lib_dir.as_deref(), &extra))).await?;
    if st.installed {
        if let Some(d) = &st.lib_dir {
            // Remember the folder for connections that don't set one.
            if databrain_connector_oracle::client::default_dir().is_none() {
                let (ws, dir) = (state.workspace.clone(), d.clone());
                blocking(move || trust_oracle_client(&ws, &dir)).await?;
                databrain_connector_oracle::client::set_default_dir(Some(d.clone()));
                state.workspace.set_setting(ORACLE_CLIENT_SETTING, &serde_json::json!(d))?;
            }
        }
    }
    Ok(st)
}

/// UI event channel for Instant Client install progress.
pub const ORACLE_INSTALL_EVENT: &str = "oracle-install";

/// Set to stop a running Instant Client download.
#[cfg(feature = "oracle")]
static ORACLE_INSTALL_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "oracle")]
pub fn oracle_cancel_install() {
    ORACLE_INSTALL_CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Download, verify and install Instant Client (Basic) for this computer
/// into `<app data>/oracle/instantclient_<ver>`. Progress is emitted on
/// [`ORACLE_INSTALL_EVENT`].
#[cfg(feature = "oracle")]
pub async fn oracle_install_client(state: &AppState, app_dir: PathBuf) -> Result<databrain_connector_oracle::client::ClientStatus> {
    let dest = app_dir.join("oracle");
    let ui = state.ui.clone();
    ORACLE_INSTALL_CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
    let dir = blocking(move || {
        let progress = |p: databrain_connector_oracle::client::InstallProgress| ui.emit(ORACLE_INSTALL_EVENT, serde_json::to_value(&p).unwrap_or_default());
        databrain_connector_oracle::client::install(&dest, &progress, &ORACLE_INSTALL_CANCEL).map_err(|e| {
            if e == "cancelled" { EngineError::new("cancelled", "download cancelled") } else { EngineError::new("install", e) }
        })
    })
    .await?;
    let d = dir.to_string_lossy().into_owned();
    // The installed libraries, as unpacked from the verified package.
    let (ws, installed) = (state.workspace.clone(), d.clone());
    blocking(move || trust_oracle_client(&ws, &installed)).await?;
    databrain_connector_oracle::client::set_default_dir(Some(d.clone()));
    state.workspace.set_setting(ORACLE_CLIENT_SETTING, &serde_json::json!(d))?;
    oracle_client_status(state, Some(d), Some(app_dir)).await
}
