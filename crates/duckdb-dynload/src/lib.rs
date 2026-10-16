//! DuckDB without the bundled engine: the `duckdb` crate is built against
//! libduckdb-sys without `bundled`, and every C API function it calls is
//! defined here as a trampoline into the DuckDB library found on the
//! computer at runtime (`libduckdb.dylib` / `libduckdb.so` / `duckdb.dll`).
//!
//! Nothing links to the library at build time, so the app starts without
//! it; until [`load`] succeeds the DuckDB connector reports the engine as
//! missing (and must not call into DuckDB).

#[cfg(feature = "system")]
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
#[cfg(feature = "system")]
use std::sync::atomic::{AtomicPtr, Ordering};

#[cfg(feature = "system")]
mod api;

/// DuckDB versions older than this lack functions the `duckdb` crate uses.
pub const MIN_VERSION: (u32, u32) = (1, 5);

#[cfg(feature = "system")]
static SYMS: [AtomicPtr<c_void>; api::NAMES.len()] = [const { AtomicPtr::new(std::ptr::null_mut()) }; api::NAMES.len()];
static LOADED: OnceLock<Loaded> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct Loaded {
    pub path: PathBuf,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct LoadError {
    /// Each place looked at, with why it failed.
    pub tried: Vec<(PathBuf, String)>,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.tried.iter().find(|(_, e)| e != NOT_FOUND) {
            Some((p, e)) => write!(f, "{}: {e}", p.display()),
            None => write!(f, "the DuckDB library ({}) was not found", lib_file_name()),
        }
    }
}

#[cfg_attr(not(feature = "system"), allow(dead_code))]
const NOT_FOUND: &str = "not found";

#[cfg(feature = "system")]
pub(crate) fn sym(i: usize) -> *mut c_void {
    let p = SYMS[i].load(Ordering::Relaxed);
    if p.is_null() {
        // A function the installed DuckDB doesn't have (or nothing loaded):
        // there is no safe way to continue a call into DuckDB.
        eprintln!("DataBrain: {} is missing from the installed DuckDB library; install DuckDB {}.{} or newer", api::NAMES[i], MIN_VERSION.0, MIN_VERSION.1);
        std::process::abort();
    }
    p
}

/// The library loaded so far, if any.
pub fn loaded() -> Option<&'static Loaded> {
    LOADED.get()
}

pub fn lib_file_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "libduckdb.dylib"
    } else if cfg!(windows) {
        "duckdb.dll"
    } else {
        "libduckdb.so"
    }
}

/// Where to look: `explicit` (a file, or a folder holding it), then
/// `DATABRAIN_DUCKDB_LIB`, then the usual install places.
pub fn candidates(explicit: Option<&Path>) -> Vec<PathBuf> {
    let name = lib_file_name();
    let mut out = Vec::new();
    let mut push = |p: PathBuf| {
        let p = if p.is_dir() { p.join(name) } else { p };
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(p) = explicit {
        push(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("DATABRAIN_DUCKDB_LIB") {
        push(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
    let dirs: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec!["/opt/homebrew/lib".into(), "/opt/homebrew/opt/duckdb/lib".into(), "/usr/local/lib".into(), "/usr/local/opt/duckdb/lib".into(), home.join(".duckdb/lib"), home.join("lib")]
    } else if cfg!(windows) {
        let pf = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        let mut v = vec![pf.join("DuckDB"), pf.join("duckdb"), home.join(".duckdb").join("lib")];
        // duckdb.dll next to a duckdb.exe on PATH (winget, scoop, choco).
        if let Some(path) = std::env::var_os("PATH") {
            v.extend(std::env::split_paths(&path).filter(|d| d.join("duckdb.exe").is_file() || d.join("duckdb.dll").is_file()));
        }
        v
    } else {
        vec!["/usr/local/lib".into(), "/usr/lib".into(), "/usr/lib64".into(), "/usr/lib/x86_64-linux-gnu".into(), "/usr/lib/aarch64-linux-gnu".into(), "/home/linuxbrew/.linuxbrew/lib".into(), home.join(".duckdb/lib"), home.join(".local/lib")]
    };
    for d in dirs {
        push(d.join(name));
    }
    out
}

/// Load the first DuckDB library that works. Once one is loaded, later
/// calls return it (a process can't switch libraries).
pub fn load(explicit: Option<&Path>) -> Result<&'static Loaded, LoadError> {
    if let Some(l) = LOADED.get() {
        return Ok(l);
    }
    #[cfg(not(feature = "system"))]
    {
        let _ = explicit;
        Err(LoadError { tried: vec![(PathBuf::new(), "this build has DuckDB built in".into())] })
    }
    #[cfg(feature = "system")]
    load_system(explicit)
}

#[cfg(feature = "system")]
fn load_system(explicit: Option<&Path>) -> Result<&'static Loaded, LoadError> {
    let mut tried = Vec::new();
    for p in candidates(explicit) {
        if !p.is_file() {
            tried.push((p, NOT_FOUND.to_string()));
            continue;
        }
        match open(&p) {
            Ok(l) => return Ok(LOADED.get_or_init(|| l)),
            Err(e) => tried.push((p, e)),
        }
    }
    Err(LoadError { tried })
}

#[cfg(feature = "system")]
fn open(path: &Path) -> Result<Loaded, String> {
    // SAFETY: loading a shared library runs its initialisers; DuckDB's are benign.
    let lib = unsafe { libloading::Library::new(path) }.map_err(|e| e.to_string())?;
    let get = |name: &str| -> *mut c_void {
        let mut z = name.as_bytes().to_vec();
        z.push(0);
        // SAFETY: only the address is read; it is called with the bindings' signature.
        unsafe { lib.get::<*mut c_void>(&z) }.map(|s| *s).unwrap_or(std::ptr::null_mut())
    };
    let ver = get("duckdb_library_version");
    if ver.is_null() {
        return Err("not a DuckDB library (no duckdb_library_version)".into());
    }
    // SAFETY: `const char *duckdb_library_version(void)`, a static string.
    let version = unsafe {
        let f: unsafe extern "C" fn() -> *const std::os::raw::c_char = std::mem::transmute(ver);
        std::ffi::CStr::from_ptr(f()).to_string_lossy().into_owned()
    };
    let (maj, min) = parse_version(&version).ok_or_else(|| format!("unknown DuckDB version {version}"))?;
    if (maj, min) < MIN_VERSION {
        return Err(format!("DuckDB {version} is too old; DataBrain needs {}.{} or newer", MIN_VERSION.0, MIN_VERSION.1));
    }
    let mut missing = 0;
    for (i, n) in api::NAMES.iter().enumerate() {
        let p = get(n);
        missing += usize::from(p.is_null());
        SYMS[i].store(p, Ordering::Relaxed);
    }
    if get("duckdb_open_ext").is_null() || get("duckdb_query_arrow").is_null() && get("duckdb_execute_prepared").is_null() {
        return Err(format!("DuckDB {version} is missing core functions ({missing} of {} not found)", api::NAMES.len()));
    }
    // Kept for the life of the process: the trampolines point into it.
    std::mem::forget(lib);
    Ok(Loaded { path: path.to_path_buf(), version })
}

/// `v1.5.6` / `1.5.6-dev123` → (1, 5).
#[cfg_attr(not(feature = "system"), allow(dead_code))]
fn parse_version(v: &str) -> Option<(u32, u32)> {
    let mut it = v.trim().trim_start_matches('v').split(['.', '-']);
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_versions() {
        assert_eq!(parse_version("v1.5.6"), Some((1, 5)));
        assert_eq!(parse_version("1.10.0-dev12"), Some((1, 10)));
        assert!(parse_version("x").is_none());
        assert!((1, 4) < MIN_VERSION && (1, 6) >= MIN_VERSION);
    }

    #[cfg(feature = "system")]
    #[test]
    fn explicit_folder_and_missing_library() {
        let dir = std::env::temp_dir().join("databrain-dynload-test");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(candidates(Some(&dir))[0], dir.join(lib_file_name()));
        let e = load(Some(&dir.join("nope").join(lib_file_name()))).err();
        // Another test in this process may have loaded a real library.
        if let Some(e) = e {
            assert!(e.tried.iter().any(|(_, r)| r == NOT_FOUND));
        }
    }

    #[cfg(feature = "system")]
    #[test]
    fn rejects_a_library_that_is_not_duckdb() {
        let dir = std::env::temp_dir().join("databrain-dynload-test2");
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join(lib_file_name());
        std::fs::write(&fake, b"not a library").unwrap();
        if LOADED.get().is_none() {
            let e = load(Some(&fake)).unwrap_err();
            assert!(e.to_string().contains(&*fake.to_string_lossy()), "{e}");
        }
    }
}
