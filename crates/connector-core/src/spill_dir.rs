//! A folder of this process's own under a shared one, for files that must
//! not outlive it (results kept on disk, DuckDB's spill files).
//!
//! The app, the jobs runner and the MCP server can run at once and share
//! DataBrain's folder. Each claims `<base>/<pid>-<nanos>` and holds a lock on
//! its `lock` file while it runs; folders nobody holds (left by a process
//! that quit or crashed) are deleted when another one claims its own.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A process folder and the lock that marks it as in use.
#[derive(Debug)]
pub struct ProcessDir {
    path: PathBuf,
    _lock: File,
}

impl ProcessDir {
    /// Delete the folders under `base` nobody holds, then create and lock
    /// one for this process.
    pub fn claim(base: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(base)?;
        remove_unheld(base);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let path = base.join(format!("{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(path.join("lock"))?;
        lock.lock()?;
        Ok(Self { path, _lock: lock })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Remove the folders under `base` whose lock nobody holds. A process
/// starting right now may not have locked its folder yet: only folders
/// untouched for a minute count as left behind.
fn remove_unheld(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else { return };
    for e in entries.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let held = OpenOptions::new().write(true).open(dir.join("lock")).is_ok_and(|f| f.try_lock().is_err());
        if !held && !fresh(&dir) && !fresh(&dir.join("lock")) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

fn fresh(p: &Path) -> bool {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age < Duration::from_secs(60))
}
