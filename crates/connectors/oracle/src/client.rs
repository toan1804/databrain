//! Oracle Instant Client: detection, the process-wide library directory and
//! a one-click installer (macOS DMG via `hdiutil` + Oracle's `install_ic.sh`;
//! Windows/Linux ZIP via `tar`/`unzip`). Downloads use Oracle's permanent
//! "latest Basic package" links over HTTPS with the system `curl`.

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
    /// Permanent link to the latest Basic package (None: no build for it).
    pub download_url: Option<&'static str>,
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
            download_url: Some("https://download.oracle.com/otn_software/mac/instantclient/instantclient-basic-macos-arm64.dmg"),
            auto_install: true,
            note: None,
        },
        ("macos", _) => Platform {
            os: "macos",
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/macos-intel-x86-downloads.html",
            download_url: Some("https://download.oracle.com/otn_software/mac/instantclient/instantclient-basic-macos.dmg"),
            auto_install: true,
            note: None,
        },
        ("windows", _) => Platform {
            os: "windows",
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/winx64-64-downloads.html",
            download_url: Some("https://download.oracle.com/otn_software/nt/instantclient/instantclient-basic-windows.zip"),
            auto_install: true,
            note: Some("Instant Client also needs the Microsoft Visual C++ Redistributable."),
        },
        ("linux", "aarch64") => Platform {
            os: "linux",
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/linux-arm-aarch64-downloads.html",
            download_url: Some("https://download.oracle.com/otn_software/linux/instantclient/instantclient-basic-linux-arm64.zip"),
            auto_install: true,
            note: Some("Also install libaio (e.g. sudo apt install libaio1 or sudo dnf install libaio)."),
        },
        ("linux", _) => Platform {
            os: "linux",
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/linux-x86-64-downloads.html",
            download_url: Some("https://download.oracle.com/otn_software/linux/instantclient/instantclient-basic-linuxx64.zip"),
            auto_install: true,
            note: Some("Also install libaio (e.g. sudo apt install libaio1 or sudo dnf install libaio)."),
        },
        (os, _) => Platform {
            os: if os == "freebsd" { "freebsd" } else { "other" },
            arch,
            download_page: "https://www.oracle.com/database/technologies/instant-client/downloads.html",
            download_url: None,
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
        if root.ends_with("oracle") {
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.filter_map(|e| e.ok()) {
                    consider(e.path().join("client64").join("lib"));
                }
            }
        }
    }
    // Library search path.
    let var = if cfg!(target_os = "windows") { "PATH" } else if cfg!(target_os = "macos") { "DYLD_LIBRARY_PATH" } else { "LD_LIBRARY_PATH" };
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
    /// Directory in use or found.
    pub lib_dir: Option<String>,
    pub message: Option<String>,
    pub platform: Platform,
}

/// Point the Oracle driver at `dir` (or the default/auto-detected folder)
/// unless it is already initialized. Retries after a failed attempt.
pub fn init_client(dir: Option<&str>, extra: &[PathBuf]) -> Result<(), String> {
    if oracle::InitParams::is_initialized() {
        return Ok(());
    }
    let chosen = dir
        .map(str::to_string)
        .filter(|d| !d.trim().is_empty())
        .or_else(default_dir)
        .or_else(|| candidate_dirs(extra).first().map(|p| p.to_string_lossy().into_owned()));
    let mut p = oracle::InitParams::new();
    if let Some(d) = &chosen {
        p.oracle_client_lib_dir(d.as_str()).map_err(|e| e.to_string())?;
    }
    p.init().map(|_| ()).map_err(|e| e.to_string())
}

/// Is Instant Client usable? Initializes the driver when it is found.
pub fn status(dir: Option<&str>, extra: &[PathBuf]) -> ClientStatus {
    let platform = platform();
    let lib_dir = dir
        .map(str::to_string)
        .filter(|d| !d.trim().is_empty())
        .or_else(default_dir)
        .or_else(|| candidate_dirs(extra).first().map(|p| p.to_string_lossy().into_owned()));
    match init_client(lib_dir.as_deref(), extra).and_then(|_| oracle::Version::client().map_err(|e| e.to_string())) {
        Ok(v) => ClientStatus { installed: true, version: Some(v.to_string()), lib_dir, message: None, platform },
        Err(e) => ClientStatus {
            installed: false,
            version: None,
            message: Some(if e.contains("DPI-1047") || e.contains("Cannot locate") {
                "Oracle Instant Client was not found on this computer.".to_string()
            } else {
                e
            }),
            lib_dir,
            platform,
        },
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

/// Download and install the latest Basic package into `dest`
/// (`<dest>/instantclient_<ver>`). Returns the library directory.
///
/// macOS: the DMG is mounted privately and its files are copied with
/// symlinks preserved (Oracle's `install_ic.sh` only works for disks mounted
/// under /Volumes, and ~/Downloads may be off-limits to apps).
pub fn install(dest: &Path, progress: &dyn Fn(InstallProgress), cancel: &AtomicBool) -> Result<PathBuf, String> {
    let p = platform();
    let url = p.download_url.ok_or("no Instant Client build for this platform; use the download page")?;
    let tmp = std::env::temp_dir().join(format!("databrain-instantclient-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let file = tmp.join(url.rsplit('/').next().unwrap_or("instantclient"));
    let result = (|| {
        download(url, &file, progress, cancel)?;
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
                    .filter(|p| !matches!(p.file_name().and_then(|n| n.to_str()), Some("install_ic.sh" | "INSTALL_IC_README.txt")) && !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')))
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
        let found = candidate_dirs(&[dir.clone()]);
        assert_eq!(&found[..2], &[new.clone(), old.clone()]);
        assert!(!found.contains(&dir.join("instantclient_empty")));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(folder_name("Basic Package\n\nClient Shared Library 64-bit - 23.3.0.23.09\n").as_deref(), Some("instantclient_23_3"));
        assert_eq!(folder_name("Client Shared Library 64-bit - 19.8.0.0.0"), Some("instantclient_19_8".into()));
        assert_eq!(folder_name("nothing here"), None);
        let p = platform();
        assert!(p.download_page.starts_with("https://www.oracle.com/"));
        assert!(p.download_url.is_none_or(|u| u.starts_with("https://download.oracle.com/")));
    }
}

#[cfg(test)]
mod live {
    /// `DATABRAIN_ORACLE_CLIENT_LIVE=1 cargo test -p databrain-connector-oracle client_status_live -- --nocapture`
    #[test]
    fn client_status_live() {
        if std::env::var("DATABRAIN_ORACLE_CLIENT_LIVE").is_err() {
            return;
        }
        let s = super::status(None, &[]);
        eprintln!("installed={} version={:?} dir={:?} msg={:?} auto_install={}", s.installed, s.version, s.lib_dir, s.message, s.platform.auto_install);
    }
}

#[cfg(test)]
mod live_install {
    /// `DATABRAIN_ORACLE_INSTALL_LIVE=/tmp/dir cargo test -p databrain-connector-oracle install_live -- --nocapture`
    /// Downloads ~115 MB from Oracle.
    #[test]
    fn install_live() {
        let Ok(dest) = std::env::var("DATABRAIN_ORACLE_INSTALL_LIVE") else { return };
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
