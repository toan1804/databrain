//! `.sql` files on disk edited in query tabs: read, save (atomically, and
//! refusing to overwrite a file changed by another program since it was
//! read), and the modification time used to notice outside changes.
//!
//! Paths come from the open/save dialogs (and tabs restored from them); only
//! text files up to [`MAX_FILE_BYTES`] are read.

use std::path::Path;

use databrain_query_engine::{EngineError, Result};
use serde::Serialize;

/// Largest file opened in the editor.
pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct SqlFile {
    pub path: String,
    pub text: String,
    /// Modification time (ms since the epoch).
    pub mtime_ms: i64,
}

fn io(path: &Path, e: std::io::Error) -> EngineError {
    let what = match e.kind() {
        std::io::ErrorKind::NotFound => "not found".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => e.to_string(),
    };
    EngineError::new("io", format!("{}: {what}", path.display()))
}

fn mtime(path: &Path) -> Result<i64> {
    let m = std::fs::metadata(path).map_err(|e| io(path, e))?;
    Ok(m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0))
}

/// Modification time of a file, `None` when it is gone.
pub fn file_mtime(path: &str) -> Option<i64> {
    mtime(Path::new(path)).ok()
}

pub fn read_sql_file(path: &str) -> Result<SqlFile> {
    let p = Path::new(path);
    let meta = std::fs::metadata(p).map_err(|e| io(p, e))?;
    if !meta.is_file() {
        return Err(EngineError::new("invalid", format!("{path} is not a file")));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(EngineError::new("invalid", format!("{path} is larger than {} MB", MAX_FILE_BYTES / 1024 / 1024)));
    }
    let bytes = std::fs::read(p).map_err(|e| io(p, e))?;
    let text = String::from_utf8(bytes).map_err(|_| EngineError::new("invalid", format!("{path} is not a UTF-8 text file")))?;
    // A byte-order mark is not part of the SQL.
    let text = text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text);
    Ok(SqlFile { path: path.to_string(), text, mtime_ms: mtime(p)? })
}

/// Write `text` to `path` (through a temporary file renamed over it).
/// `expected_mtime`: the time the file had when read; a different one
/// means another program changed it, and nothing is written.
pub fn write_sql_file(path: &str, text: &str, expected_mtime: Option<i64>) -> Result<i64> {
    let p = Path::new(path);
    if let (Some(want), Ok(now)) = (expected_mtime, mtime(p)) {
        if want != now {
            return Err(EngineError::new("conflict", format!("{} was changed by another program since it was opened", p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| path.into()))));
        }
    }
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "file.sql".into());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| io(&tmp, e))?;
    // Keep the original file's permissions.
    if let Ok(m) = std::fs::metadata(p) {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    if let Err(e) = std::fs::rename(&tmp, p) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(p, e));
    }
    mtime(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_saves_and_detects_outside_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.sql");
        std::fs::write(&path, "\u{feff}select 1;\n").unwrap();
        let p = path.to_string_lossy().into_owned();
        let f = read_sql_file(&p).unwrap();
        assert_eq!(f.text, "select 1;\n", "BOM dropped");
        let t1 = write_sql_file(&p, "select 2;\n", Some(f.mtime_ms)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 2;\n");
        // Another program writes it: saving with the old time is refused.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "select 3;\n").unwrap();
        let e = write_sql_file(&p, "select 4;\n", Some(t1 - 1)).unwrap_err();
        assert_eq!(e.kind, "conflict");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 3;\n");
        // Save as (no expected time) and new files.
        let n = dir.path().join("new.sql").to_string_lossy().into_owned();
        write_sql_file(&n, "x", None).unwrap();
        assert_eq!(read_sql_file(&n).unwrap().text, "x");
        assert!(read_sql_file(&dir.path().to_string_lossy()).is_err(), "a folder");
        std::fs::write(dir.path().join("bin.sql"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(read_sql_file(&dir.path().join("bin.sql").to_string_lossy()).unwrap_err().message.contains("UTF-8"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".tmp")).count(), 0);
    }
}
