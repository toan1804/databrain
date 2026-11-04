//! Oracle Instant Client: detection, the process-wide library directory and
//! a one-click installer (macOS DMG via `hdiutil` + Oracle's `install_ic.sh`;
//! Windows/Linux ZIP via `tar`/`unzip`). Downloads a pinned Basic package
//! over HTTPS with the system `curl` and checks it against the SHA-256 that
//! Oracle publishes for it (built into DataBrain) before installing. The
//! libraries of the folder DataBrain installed or adopted are hashed then
//! ([`manifest`]) and checked again before they are loaded ([`set_trusted`]).
//!
//! Updating Instant Client: take the versioned link and SHA-256 of each
//! platform's Basic package from Oracle's download pages (`download_page`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

/// Directory used when a connection has no "Instant Client directory".
static DEFAULT_DIR: RwLock<Option<String>> = RwLock::new(None);

pub fn set_default_dir(dir: Option<String>) {
    if let Ok(mut g) = DEFAULT_DIR.write() {
        *g = dir.filter(|d| !d.trim().is_empty());
    }
}

pub fn default_dir() -> Option<String> {
    DEFAULT_DIR.read().ok().and_then(|g| g.clone())
}

/// File name of the OCI library on this platform.
pub const fn library_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "oci.dll"
    } else if cfg!(target_os = "macos") {
        "libclntsh.dylib"
    } else {
        "libclntsh.so"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
    /// Oracle's download page for this platform.
    pub download_page: &'static str,
    /// The Basic package DataBrain installs (None: no build for it).
    pub download_url: Option<&'static str>,
    /// SHA-256 of `download_url`, as published by Oracle.
    pub sha256: Option<&'static str>,
    /// DataBrain can download and install it without further steps.
    pub auto_install: bool,
    /// Extra step the user must do (e.g. `libaio` on Linux).
    pub note: Option<&'static str>,
}

pub fn platform() -> Platform {
    let arch = std::env::consts::ARCH;
    match (std::env::consts::OS, arch) {
        ("macos", "aarch64") => Platform {
            os: "macos",
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/macos-arm64-downloads.html",
            download_url: Some("https://download.oracle.com/otn_software/mac/instantclient/2326300/instantclient-basic-macos.arm64-23.26.3.0.0.dmg"),
            sha256: Some("9ad4824c5dca702e6bb68d459e2cd738e6ee1e750c668e54a09a43fee9f5d08a"),
            auto_install: true,
            note: None,
        },
        ("macos", _) => Platform {
            os: "macos",
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/macos-intel-x86-downloads.html",
            download_url: Some("https://download.oracle.com/otn_software/mac/instantclient/1916000/instantclient-basic-macos.x64-19.16.0.0.0dbru.dmg"),
            sha256: Some("725eeb7c00041e926b57c3db508ea7439a1935a37f480c0ac6d46eddc4205331"),
            auto_install: true,
            note: None,
        },
        ("windows", _) => Platform {
            os: "windows",
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/winx64-64-downloads.html",
            download_url: Some("https://download.oracle.com/otn_software/nt/instantclient/2326400/instantclient-basic-windows.x64-23.26.4.0.0.zip"),
            sha256: Some("09e9b81cd492f5ce6f671317c23a0e1f5dee5e786258149d216321c3dad3023f"),
            auto_install: true,
            note: Some("Instant Client also needs the Microsoft Visual C++ Redistributable."),
        },
        ("linux", "aarch64") => Platform {
            os: "linux",
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/linux-arm-aarch64-downloads.html",
            download_url: Some("https://download.oracle.com/otn_software/linux/instantclient/2326400/instantclient-basic-linux.arm64-23.26.4.0.0.zip"),
            sha256: Some("688df426cabb4e5945e8409ae022acd16b92ff06582c8d93af2f41e3fb9a42f3"),
            auto_install: true,
            note: Some("Also install libaio (e.g. sudo apt install libaio1 or sudo dnf install libaio)."),
        },
        ("linux", _) => Platform {
            os: "linux",
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/linux-x86-64-downloads.html",
            download_url: Some("https://download.oracle.com/otn_software/linux/instantclient/2326400/instantclient-basic-linux.x64-23.26.4.0.0.zip"),
            sha256: Some("cab59aa3b411b3b50898e456fa9aabfe48d67e25cbc1bb23303800390ce7de26"),
            auto_install: true,
            note: Some("Also install libaio (e.g. sudo apt install libaio1 or sudo dnf install libaio)."),
        },
        (os, _) => Platform {
            os: if os == "freebsd" { "freebsd" } else { "other" },
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/downloads.html",
            download_url: None,
            sha256: None,
            auto_install: false,
            note: None,
        },
    }
}

fn has_lib(dir: &Path) -> bool {
    dir.join(library_name()).is_file()
}

/// Folders where Instant Client usually lives (existing ones only), newest
/// version first. `extra` (e.g. DataBrain's own install folder) comes first.
pub fn candidate_dirs(extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = extra.to_vec();
    if let Some(h) = dirs_home() {
        roots.push(h.join("Downloads"));
        roots.push(h.clone());
        roots.push(h.join("oracle"));
        roots.push(h.join("Library").join("Oracle"));
    }
    for r in ["/opt/oracle", "/usr/local/oracle", "/usr/lib/oracle", "/usr/local/lib", "C:\\oracle", "C:\\instantclient"] {
        roots.push(PathBuf::from(r));
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut consider = |d: PathBuf| {
        if has_lib(&d) && !found.contains(&d) {
            found.push(d);
        }
    };
    for root in roots {
        consider(root.clone());
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        let mut subs: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_ascii_lowercase().starts_with("instantclient")))
            .collect();
        subs.sort();
        subs.reverse();
        for s in subs {
            consider(s);
        }
        // /usr/lib/oracle/<ver>/client64/lib (Linux RPM layout)
        if root.ends_with("oracle")
            && let Ok(rd) = std::fs::read_dir(&root)
        {
            for e in rd.filter_map(|e| e.ok()) {
                consider(e.path().join("client64").join("lib"));
            }
        }
    }
    // Library search path.
    let var = if cfg!(target_os = "windows") {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    if let Some(p) = std::env::var_os(var) {
        for d in std::env::split_paths(&p) {
            consider(d);
        }
    }
    found
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(target_os = "windows") { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientStatus {
    pub installed: bool,
    /// e.g. "23.26.2.0.0"
    pub version: Option<String>,
    /// Directory in use or checked.
    pub lib_dir: Option<String>,
    /// A folder with Instant Client found on the computer but not loaded:
    /// DataBrain loads only a folder the user chose or it installed.
    pub found: Option<String>,
    pub message: Option<String>,
    pub platform: Platform,
}

/// Point the Oracle driver at `dir` (or the default/auto-detected folder)
/// unless it is already initialized. Retries after a failed attempt.
///
/// The folder DataBrain installed or adopted ([`set_trusted`]) is checked
/// against its manifest first: its libraries are loaded into this process,
/// so a folder whose libraries changed since is not used.
pub fn init_client(dir: Option<&str>, extra: &[PathBuf]) -> Result<(), InitError> {
    if oracle::InitParams::is_initialized() {
        return Ok(());
    }
    let chosen = chosen_dir(dir, extra);
    // No folder chosen: nothing is loaded (not even from the system's
    // library search path, which a download can reach).
    let Some(d) = chosen else {
        return Err(InitError::Load("DPI-1047: Cannot locate Oracle Instant Client: choose its folder or install it".into()));
    };
    let mut p = oracle::InitParams::new();
    check_trusted(&d).map_err(InitError::Changed)?;
    p.oracle_client_lib_dir(d.as_str()).map_err(|e| InitError::Load(e.to_string()))?;
    p.init().map(|_| ()).map_err(|e| InitError::Load(e.to_string()))
}

/// Why Instant Client was not initialized.
#[derive(Debug, Clone, PartialEq)]
pub enum InitError {
    /// The checked folder's libraries changed ([`verify`]): nothing may be
    /// loaded for it, not even from the default search path.
    Changed(String),
    /// Not found or not loadable (retried on the next connect).
    Load(String),
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitError::Changed(m) | InitError::Load(m) => f.write_str(m),
        }
    }
}

/// The folder Instant Client is loaded from: `dir` (the connection's),
/// the default folder (chosen by the user or installed by DataBrain), or
/// one in DataBrain's own folders (`extra`). Never a folder that merely
/// exists elsewhere (Downloads, the home folder, the library path): its
/// libraries would be loaded into the app, and a downloaded file is not a
/// choice.
fn chosen_dir(dir: Option<&str>, extra: &[PathBuf]) -> Option<String> {
    dir.map(str::to_string)
        .filter(|d| !d.trim().is_empty())
        .or_else(default_dir)
        .or_else(|| candidate_dirs(extra).into_iter().find(|c| extra.iter().any(|e| c.starts_with(e))).map(|p| p.to_string_lossy().into_owned()))
}

/// SHA-256 (hex) of each shared library in an Instant Client folder, by file name.
pub type Manifest = std::collections::BTreeMap<String, String>;

/// The folder whose libraries DataBrain checked, and their hashes.
static TRUSTED: RwLock<Option<(PathBuf, Manifest)>> = RwLock::new(None);

/// Libraries of `dir` must match `manifest` before Instant Client is loaded
/// from it (`None`: no folder is checked).
pub fn set_trusted(trusted: Option<(String, Manifest)>) {
    if let Ok(mut g) = TRUSTED.write() {
        *g = trusted.map(|(d, m)| (comparable(Path::new(&d)), m));
    }
}

fn comparable(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Shared libraries (`libclntsh.dylib.23.1`, `libnnz.so`, `oci.dll`): what
/// the driver loads. Other files (tools, jars, readmes) are not checked.
fn is_library(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".dll") || n.ends_with(".dylib") || n.contains(".dylib.") || n.ends_with(".so") || n.contains(".so.")
}

/// Hash every shared library in `dir` (links are followed: what loads is
/// the file they point to).
pub fn manifest(dir: &Path) -> Result<Manifest, String> {
    let mut m = Manifest::new();
    for e in std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if is_library(&name) && e.path().is_file() {
            m.insert(name, file_sha256(&e.path())?);
        }
    }
    if !m.contains_key(library_name()) {
        return Err(format!("{} has no {}", dir.display(), library_name()));
    }
    Ok(m)
}

/// `dir`'s libraries are the ones in `want`: none changed, removed or added.
pub fn verify(dir: &Path, want: &Manifest) -> Result<(), String> {
    let got = manifest(dir).unwrap_or_default();
    let mut differ: Vec<&str> = want.iter().filter(|(n, h)| got.get(*n) != Some(h)).map(|(n, _)| n.as_str()).collect();
    differ.extend(got.keys().filter(|n| !want.contains_key(*n)).map(String::as_str));
    if differ.is_empty() {
        return Ok(());
    }
    differ.sort_unstable();
    Err(format!(
        "Oracle Instant Client in {} changed since DataBrain checked it ({}), so it is not loaded. Install Instant Client again from the connection's Driver section, or choose the folder again.",
        dir.display(),
        differ.join(", ")
    ))
}

/// [`verify`] `dir` if it is the checked folder.
fn check_trusted(dir: &str) -> Result<(), String> {
    let g = TRUSTED.read().map_err(|_| "lock poisoned".to_string())?;
    match g.as_ref() {
        Some((d, m)) if *d == comparable(Path::new(dir)) => verify(d, m),
        _ => Ok(()),
    }
}

/// Is Instant Client usable? Initializes the driver when it is found.
pub fn status(dir: Option<&str>, extra: &[PathBuf]) -> ClientStatus {
    let platform = platform();
    let lib_dir = chosen_dir(dir, extra);
    // Found elsewhere: shown, not loaded (the user chooses it).
    let found = || if lib_dir.is_none() { candidate_dirs(&[]).first().map(|p| p.to_string_lossy().into_owned()) } else { None };
    match init_client(lib_dir.as_deref(), extra).map_err(|e| e.to_string()).and_then(|_| oracle::Version::client().map_err(|e| e.to_string())) {
        Ok(v) => ClientStatus { installed: true, version: Some(v.to_string()), lib_dir, found: None, message: None, platform },
        Err(e) => {
            let found = found();
            ClientStatus {
                installed: false,
                version: None,
                message: Some(match (&found, e.contains("DPI-1047") || e.contains("Cannot locate")) {
                    (Some(f), true) => format!("Oracle Instant Client is in {f}, but DataBrain only loads a folder you choose: use it if you trust it."),
                    (None, true) => "Oracle Instant Client was not found on this computer.".to_string(),
                    _ => e,
                }),
                lib_dir,
                found,
                platform,
            }
        }
    }
}

fn run(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{what} failed: {}", err.trim().lines().last().unwrap_or("unknown error")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Install progress reported to the UI.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum InstallProgress {
    Downloading { received: u64, total: Option<u64> },
    Installing,
    Verifying,
}

/// `instantclient_23_3` from BASIC_README ("Client Shared Library 64-bit - 23.3.0.23.09").
pub fn folder_name(readme: &str) -> Option<String> {
    let line = readme.lines().find(|l| l.contains("Client Shared Library"))?;
    let ver = line.rsplit(" - ").next()?.trim();
    let mut it = ver.split('.');
    let (major, minor) = (it.next()?, it.next()?);
    (major.chars().all(|c| c.is_ascii_digit()) && minor.chars().all(|c| c.is_ascii_digit())).then(|| format!("instantclient_{major}_{minor}"))
}

fn content_length(url: &str) -> Option<u64> {
    let out = Command::new("curl").args(["-sIL", "--proto", "=https", "--max-time", "20"]).arg(url).output().ok()?;
    // Last header block wins (after redirects).
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .rev()
        .filter_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse().ok()))
        .next()
}

/// curl to `file`, reporting the growing file size; stops when `cancel` is set.
fn download(url: &str, file: &Path, progress: &dyn Fn(InstallProgress), cancel: &AtomicBool) -> Result<(), String> {
    let total = content_length(url);
    progress(InstallProgress::Downloading { received: 0, total });
    let mut child = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--retry", "2", "-o"])
        .arg(file)
        .arg(url)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("download: {e}"))?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled".into());
        }
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            if !st.success() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut err);
                }
                return Err(format!("download failed: {}", err.trim().lines().last().unwrap_or("network error")));
            }
            let received = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
            progress(InstallProgress::Downloading { received, total: total.or(Some(received)) });
            return Ok(());
        }
        let received = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        progress(InstallProgress::Downloading { received, total });
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// SHA-256 of a file, read in chunks (packages are 100+ MB).
fn file_sha256(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h).map_err(|e| e.to_string())?;
    Ok(hex::encode(h.finalize()))
}

/// Download and install the pinned Basic package into `dest`
/// (`<dest>/instantclient_<ver>`). Returns the library directory.
///
/// macOS: the DMG is mounted privately and its files are copied with
/// symlinks preserved (Oracle's `install_ic.sh` only works for disks mounted
/// under /Volumes, and ~/Downloads may be off-limits to apps).
pub fn install(dest: &Path, progress: &dyn Fn(InstallProgress), cancel: &AtomicBool) -> Result<PathBuf, String> {
    let p = platform();
    let url = p.download_url.ok_or("no Instant Client build for this platform; use the download page")?;
    let want = p.sha256.ok_or("no checksum for this platform's Instant Client; use the download page")?;
    let tmp = std::env::temp_dir().join(format!("databrain-instantclient-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let file = tmp.join(url.rsplit('/').next().unwrap_or("instantclient"));
    let result = (|| {
        download(url, &file, progress, cancel)?;
        // Nothing is mounted or extracted before the package is the one Oracle published.
        let got = file_sha256(&file)?;
        if !got.eq_ignore_ascii_case(want) {
            return Err(format!("the downloaded Instant Client is not the package Oracle published (SHA-256 {got}, expected {want}); not installed"));
        }
        progress(InstallProgress::Installing);
        let lib_dir = if p.os == "macos" {
            let mount = tmp.join("mnt");
            std::fs::create_dir_all(&mount).map_err(|e| e.to_string())?;
            run(Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"]).arg(&mount).arg(&file), "mount")?;
            let copied = (|| {
                let readme = std::fs::read_to_string(mount.join("BASIC_README")).unwrap_or_default();
                let target = dest.join(folder_name(&readme).unwrap_or_else(|| "instantclient".into()));
                std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
                // `cp -R -P` keeps libclntsh.dylib -> libclntsh.dylib.23.1 symlinks.
                let entries: Vec<PathBuf> = std::fs::read_dir(&mount)
                    .map_err(|e| e.to_string())?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        !matches!(p.file_name().and_then(|n| n.to_str()), Some("install_ic.sh" | "INSTALL_IC_README.txt"))
                            && !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'))
                    })
                    .collect();
                if entries.is_empty() {
                    return Err("the downloaded package is empty".to_string());
                }
                run(Command::new("cp").args(["-R", "-P", "-f"]).args(&entries).arg(&target), "copy")?;
                // Writable copies so a later reinstall can overwrite them.
                let _ = Command::new("chmod").args(["-R", "u+w"]).arg(&target).output();
                Ok(target)
            })();
            let _ = Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&mount).output();
            copied?
        } else {
            if p.os == "windows" {
                run(Command::new("tar").arg("-xf").arg(&file).arg("-C").arg(dest), "extract")?;
            } else if run(Command::new("unzip").args(["-oq"]).arg(&file).arg("-d").arg(dest), "extract").is_err() {
                run(Command::new("python3").args(["-m", "zipfile", "-e"]).arg(&file).arg(dest), "extract (needs unzip or python3)")?;
            }
            dest.to_path_buf()
        };
        progress(InstallProgress::Verifying);
        if has_lib(&lib_dir) {
            return Ok(lib_dir);
        }
        candidate_dirs(&[lib_dir.clone(), dest.to_path_buf()])
            .into_iter()
            .next()
            .ok_or_else(|| format!("installed into {}, but {} was not found there", lib_dir.display(), library_name()))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a chosen folder, the default one or one in DataBrain's own
    /// folders is loaded; whatever is in Downloads, the home folder or the
    /// library path is found, never chosen.
    #[test]
    fn loads_only_chosen_or_own_folders() {
        let own = std::env::temp_dir().join(format!("dbtest-ic-own-{}", std::process::id()));
        let ic = own.join("instantclient_23_26");
        std::fs::create_dir_all(&ic).unwrap();
        std::fs::write(ic.join(library_name()), b"").unwrap();
        set_default_dir(None);
        assert_eq!(chosen_dir(None, &[]), None, "nothing chosen: nothing found elsewhere is used");
        assert_eq!(chosen_dir(None, std::slice::from_ref(&own)).map(PathBuf::from), Some(ic.clone()), "DataBrain's own install folder");
        assert_eq!(chosen_dir(Some("/x/chosen"), std::slice::from_ref(&own)).as_deref(), Some("/x/chosen"), "the connection's choice first");
        assert!(matches!(init_client(None, &[]), Err(InitError::Load(_)) | Ok(())), "never loads an unchosen folder");
        let _ = std::fs::remove_dir_all(&own);
    }

    #[test]
    fn finds_instantclient_folders() {
        let dir = std::env::temp_dir().join(format!("dbtest-ic-{}", std::process::id()));
        let old = dir.join("instantclient_19_8");
        let new = dir.join("instantclient_23_26");
        for d in [&old, &new] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join(library_name()), b"").unwrap();
        }
        std::fs::create_dir_all(dir.join("instantclient_empty")).unwrap();
        let found = candidate_dirs(std::slice::from_ref(&dir));
        assert_eq!(&found[..2], &[new.clone(), old.clone()]);
        assert!(!found.contains(&dir.join("instantclient_empty")));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(folder_name("Basic Package\n\nClient Shared Library 64-bit - 23.3.0.23.09\n").as_deref(), Some("instantclient_23_3"));
        assert_eq!(folder_name("Client Shared Library 64-bit - 19.8.0.0.0"), Some("instantclient_19_8".into()));
        assert_eq!(folder_name("nothing here"), None);
        let p = platform();
        assert!(p.download_page.starts_with("https://www.oracle.com/"));
        assert!(p.download_url.is_none_or(|u| u.starts_with("https://download.oracle.com/")));
        assert_eq!(p.download_url.is_some(), p.sha256.is_some_and(|h| h.len() == 64), "every download has its checksum");
    }

    /// The file the main library links to on Unix (`oci.dll` is the file itself).
    fn versioned() -> Option<String> {
        cfg!(unix).then(|| format!("{}.23.1", library_name()))
    }

    /// A folder like an installed Instant Client: the main library (a link
    /// to the versioned file, as installed on macOS), another library, a tool.
    fn fake_client(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        #[cfg(unix)]
        {
            let versioned = versioned().unwrap();
            std::fs::write(dir.join(&versioned), b"oci").unwrap();
            std::os::unix::fs::symlink(&versioned, dir.join(library_name())).unwrap();
        }
        #[cfg(not(unix))]
        std::fs::write(dir.join(library_name()), b"oci").unwrap();
        std::fs::write(dir.join(if cfg!(windows) { "orannzsbb.dll" } else { "libnnz.so" }), b"nnz").unwrap();
        std::fs::write(dir.join("BASIC_README"), b"readme").unwrap();
    }

    #[test]
    fn checks_the_installed_libraries_before_loading() {
        let dir = std::env::temp_dir().join(format!("dbtest-ic-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fake_client(&dir);
        let m = manifest(&dir).unwrap();
        let mut names: Vec<&str> = m.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert!(m.contains_key(library_name()), "{names:?}");
        if let Some(v) = versioned() {
            assert!(m.contains_key(&v), "{names:?}");
            assert_eq!(m[library_name()], m[&v], "a link is hashed through");
        }
        assert!(!m.contains_key("BASIC_README"), "only libraries");
        assert!(manifest(&dir.join("missing")).is_err());
        assert!(manifest(&std::env::temp_dir()).is_err_and(|e| e.contains(library_name())), "a folder without the library");

        verify(&dir, &m).unwrap();
        std::fs::write(dir.join("BASIC_README"), b"edited").unwrap();
        verify(&dir, &m).expect("other files may change");
        // Changed, added or removed libraries are refused, by name.
        let lib2 = if cfg!(windows) { "orannzsbb.dll" } else { "libnnz.so" };
        std::fs::write(dir.join(lib2), b"tampered").unwrap();
        assert!(verify(&dir, &m).is_err_and(|e| e.contains(lib2) && e.contains("not loaded")));
        std::fs::write(dir.join(lib2), b"nnz").unwrap();
        let extra = if cfg!(windows) { "evil.dll" } else { "libevil.so.1" };
        std::fs::write(dir.join(extra), b"x").unwrap();
        assert!(verify(&dir, &m).is_err_and(|e| e.contains(extra)));
        std::fs::remove_file(dir.join(extra)).unwrap();
        std::fs::remove_file(dir.join(lib2)).unwrap();
        assert!(verify(&dir, &m).is_err_and(|e| e.contains(lib2)));
        std::fs::write(dir.join(lib2), b"nnz").unwrap();

        // init_client refuses the checked folder once it changed (before
        // anything is loaded) and says why; other folders are not checked.
        set_trusted(Some((dir.to_string_lossy().into_owned(), m.clone())));
        std::fs::write(dir.join(versioned().unwrap_or_else(|| library_name().into())), b"replaced").unwrap();
        match init_client(Some(&dir.to_string_lossy()), &[]) {
            Err(InitError::Changed(e)) => assert!(e.contains(library_name()), "{e}"),
            other => panic!("{other:?}"),
        }
        let other = dir.with_extension("other");
        let _ = std::fs::remove_dir_all(&other);
        fake_client(&other);
        assert!(!matches!(init_client(Some(&other.to_string_lossy()), &[]), Err(InitError::Changed(_))));
        set_trusted(None);
        assert!(!matches!(init_client(Some(&dir.to_string_lossy()), &[]), Err(InitError::Changed(_))));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }
}

#[cfg(test)]
mod live {
    /// `cargo test -p databrain-connector-oracle client_status_live -- --ignored --nocapture`
    #[ignore = "live: reports this computer's Instant Client (cargo test -- --ignored)"]
    #[test]
    fn client_status_live() {
        let s = super::status(None, &[]);
        eprintln!("installed={} version={:?} dir={:?} msg={:?} auto_install={}", s.installed, s.version, s.lib_dir, s.message, s.platform.auto_install);
    }
}

#[cfg(test)]
mod live_install {
    /// `DATABRAIN_ORACLE_INSTALL_LIVE=/tmp/dir cargo test -p databrain-connector-oracle install_live -- --ignored --nocapture`
    /// Downloads ~115 MB from Oracle.
    #[ignore = "live: needs DATABRAIN_ORACLE_INSTALL_LIVE=<folder>, downloads ~140 MB (cargo test -- --ignored)"]
    #[test]
    fn install_live() {
        let dest = std::env::var("DATABRAIN_ORACLE_INSTALL_LIVE").expect("live test: DATABRAIN_ORACLE_INSTALL_LIVE not set (run with --ignored)");
        let seen = std::sync::Mutex::new((0u32, 0u64, None::<u64>, Vec::<String>::new()));
        let progress = |p: super::InstallProgress| {
            let mut g = seen.lock().unwrap();
            match p {
                super::InstallProgress::Downloading { received, total } => {
                    g.0 += 1;
                    g.1 = received;
                    g.2 = total;
                }
                other => g.3.push(format!("{other:?}")),
            }
        };
        let dir = super::install(std::path::Path::new(&dest), &progress, &std::sync::atomic::AtomicBool::new(false)).expect("install");
        let g = seen.lock().unwrap();
        eprintln!("dir={} updates={} received={} total={:?} phases={:?}", dir.display(), g.0, g.1, g.2, g.3);
        assert!(g.0 > 2 && g.2 == Some(g.1));
        let st = super::status(Some(&dir.to_string_lossy()), &[]);
        eprintln!("installed={} version={:?} msg={:?}", st.installed, st.version, st.message);
        assert!(st.installed);
    }
}
