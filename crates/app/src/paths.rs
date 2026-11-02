//! Checks on file paths that arrive from the webview.
//!
//! The UI only sends paths the user picked in an open/save dialog (or tabs
//! restored from them), but a command can't tell that apart from a path made
//! up by injected script. So each command checks what it is about to touch:
//! an absolute path, no symlink, no hidden folder (`~/.ssh`, `~/.zshrc`…)
//! for writes, and an extension that matches what the command produces.
//! This narrows what a compromised page could write or read; it is not a
//! sandbox (DuckDB SQL can still read local files, for instance).

use std::path::{Component, Path, PathBuf};

use databrain_query_engine::{EngineError, Result};

/// What a path is used for; decides which extensions are accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Result export (CSV, JSON, Parquet, Excel…).
    Export,
    /// Text diagram / DDL export (SVG, draw.io, Mermaid, DOT, JSON…).
    SaveText,
    /// Image export (PNG…).
    SaveImage,
    /// Connections or knowledge notes, written as JSON.
    WriteJson,
    /// Connections or knowledge notes, read from JSON.
    ReadJson,
    /// Connections imported from another tool (DataGrip XML, `.pgpass`, DBeaver JSON):
    /// any file, hidden ones included (`~/.pgpass`).
    Import,
    /// A `.sql` file saved from a query tab: any name but programs and scripts.
    WriteSql,
    /// A `.sql` file opened in a query tab, or a data file queried with DuckDB.
    Read,
}

const EXPORT: &[&str] = &["csv", "tsv", "json", "ndjson", "jsonl", "md", "sql", "parquet", "xlsx", "txt"];
const SAVE_TEXT: &[&str] = &["svg", "drawio", "xml", "mmd", "dot", "gv", "json", "md", "sql", "txt", "csv"];
const SAVE_IMAGE: &[&str] = &["png", "jpg", "jpeg", "webp"];
const JSON: &[&str] = &["json"];
/// Never written from a query tab: these run or configure something when opened.
const NOT_SQL: &[&str] = &[
    "sh", "bash", "zsh", "fish", "command", "tool", "app", "plist", "desktop", "service", "bat", "cmd", "ps1", "psm1", "vbs", "js", "jse", "wsf", "exe", "com",
    "scr", "msi", "dll", "dylib", "so", "lnk", "url", "py", "rb", "pl", "php", "jar", "reg", "profile", "rc",
];

fn invalid(msg: String) -> EngineError {
    EngineError::new("invalid", msg)
}

fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// A hidden file or folder (`.ssh`, `.zshrc`…) anywhere in `p`.
fn hidden(p: &Path) -> bool {
    p.components().any(|c| matches!(c, Component::Normal(n) if n.to_str().is_none_or(|s| s.starts_with('.'))))
}

/// Check `path` for `purpose`; returns it as a [`PathBuf`].
pub fn checked(path: &str, purpose: Purpose) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    if path.trim().is_empty() || !p.is_absolute() {
        return Err(invalid(format!("not an absolute file path: {path}")));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(invalid(format!("path with '..': {path}")));
    }
    if purpose != Purpose::Import && hidden(&p) {
        return Err(invalid(format!("DataBrain doesn't read or write hidden files and folders from here: {path}")));
    }
    let writes = matches!(purpose, Purpose::Export | Purpose::SaveText | Purpose::SaveImage | Purpose::WriteJson | Purpose::WriteSql);
    if writes {
        if let Ok(m) = std::fs::symlink_metadata(&p) {
            if m.file_type().is_symlink() {
                return Err(invalid(format!("won't write through a symbolic link: {path}")));
            }
            if m.is_dir() {
                return Err(invalid(format!("is a folder: {path}")));
            }
        }
    }
    let ext = ext_of(&p);
    let allowed: Option<&[&str]> = match purpose {
        Purpose::Export => Some(EXPORT),
        Purpose::SaveText => Some(SAVE_TEXT),
        Purpose::SaveImage => Some(SAVE_IMAGE),
        Purpose::WriteJson | Purpose::ReadJson => Some(JSON),
        Purpose::WriteSql | Purpose::Read | Purpose::Import => None,
    };
    if let Some(list) = allowed {
        if !list.contains(&ext.as_str()) {
            return Err(invalid(format!("unexpected file type '.{ext}' (expected {})", list.iter().map(|e| format!(".{e}")).collect::<Vec<_>>().join(", "))));
        }
    }
    if purpose == Purpose::WriteSql && NOT_SQL.contains(&ext.as_str()) {
        return Err(invalid(format!("won't save a query as a '.{ext}' file")));
    }
    Ok(p)
}

/// [`checked`] returning the path as text, for APIs that take `&str`.
pub fn checked_str(path: &str, purpose: Purpose) -> Result<String> {
    checked(path, purpose).map(|p| p.to_string_lossy().into_owned())
}

/// The DuckDB library chosen in Settings: a file named like the library
/// (`libduckdb.dylib`, `libduckdb.so[.N]`, `duckdb.dll`), or a folder.
pub fn checked_duckdb_library(path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        return Err(invalid(format!("not an absolute path: {path}")));
    }
    if p.is_dir() {
        return Ok(p);
    }
    let name_ok = |q: &Path| {
        let n = q.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
        n == "duckdb.dll" || (n.starts_with("libduckdb") && (n.ends_with(".dylib") || n.ends_with(".so") || n.contains(".so.")))
    };
    // Homebrew links libduckdb.dylib to a versioned file: check both names.
    let real = std::fs::canonicalize(&p).map_err(|e| invalid(format!("{path}: {e}")))?;
    if !name_ok(&p) || !name_ok(&real) {
        return Err(invalid(format!("not a DuckDB library (libduckdb.dylib, libduckdb.so or duckdb.dll): {path}")));
    }
    Ok(p)
}

/// Settings the webview may not write directly: they name code that the app
/// loads or downloads, so they change only through their own commands.
pub const PROTECTED_SETTINGS: &[&str] = &[crate::api::DUCKDB_LIBRARY_SETTING, crate::api::ORACLE_AGENT_URL_SETTING];

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(rel: &str) -> String {
        std::env::temp_dir().join(rel).to_string_lossy().into_owned()
    }

    #[test]
    fn needs_an_absolute_path_without_dots() {
        assert!(checked("out.csv", Purpose::Export).is_err());
        assert!(checked("", Purpose::Read).is_err());
        assert!(checked(&abs("a/../b.csv"), Purpose::Export).is_err());
        assert!(checked(&abs("out.csv"), Purpose::Export).is_ok());
    }

    #[test]
    fn refuses_hidden_paths_except_for_imports() {
        let home = abs("home");
        assert!(checked(&format!("{home}/.ssh/authorized_keys.json"), Purpose::WriteJson).is_err());
        assert!(checked(&format!("{home}/.zshrc"), Purpose::WriteSql).is_err());
        assert!(checked(&format!("{home}/.aws/credentials"), Purpose::Read).is_err());
        assert!(checked(&format!("{home}/.pgpass"), Purpose::Import).is_ok());
    }

    #[test]
    fn checks_the_extension_for_the_purpose() {
        assert!(checked(&abs("r.parquet"), Purpose::Export).is_ok());
        assert!(checked(&abs("r.sh"), Purpose::Export).is_err());
        assert!(checked(&abs("er.drawio"), Purpose::SaveText).is_ok());
        assert!(checked(&abs("er.plist"), Purpose::SaveText).is_err());
        assert!(checked(&abs("er.PNG"), Purpose::SaveImage).is_ok());
        assert!(checked(&abs("er.svg"), Purpose::SaveImage).is_err());
        assert!(checked(&abs("notes.json"), Purpose::ReadJson).is_ok());
        assert!(checked(&abs("notes.txt"), Purpose::WriteJson).is_err());
        assert!(checked(&abs("q.sql"), Purpose::WriteSql).is_ok());
        assert!(checked(&abs("q"), Purpose::WriteSql).is_ok());
        assert!(checked(&abs("run.command"), Purpose::WriteSql).is_err());
        assert!(checked(&abs("x.dylib"), Purpose::WriteSql).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn wont_write_through_a_symlink() {
        let dir = std::env::temp_dir().join(format!("databrain-paths-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.csv");
        std::fs::write(&target, "x").unwrap();
        let link = dir.join("link.csv");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(checked(link.to_str().unwrap(), Purpose::Export).is_err());
        assert!(checked(target.to_str().unwrap(), Purpose::Export).is_ok());
        // Reading through a link is fine (Homebrew, iCloud folders…).
        assert!(checked(link.to_str().unwrap(), Purpose::Read).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn duckdb_library_must_be_named_like_one() {
        let dir = std::env::temp_dir().join(format!("databrain-duck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join(if cfg!(target_os = "macos") { "libduckdb.dylib" } else { "libduckdb.so" });
        std::fs::write(&ok, "").unwrap();
        let bad = dir.join("evil.dylib");
        std::fs::write(&bad, "").unwrap();
        let link = dir.join("libduckdb.so.1");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&bad, &link).unwrap();
        assert!(checked_duckdb_library(ok.to_str().unwrap()).is_ok());
        assert!(checked_duckdb_library(dir.to_str().unwrap()).is_ok());
        assert!(checked_duckdb_library(bad.to_str().unwrap()).is_err());
        assert!(checked_duckdb_library(link.to_str().unwrap()).is_err(), "a well-named link to another file");
        assert!(checked_duckdb_library("libduckdb.dylib").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
