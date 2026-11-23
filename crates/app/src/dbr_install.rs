//! "Install the dbr command" (Settings → Terminal): put `dbr` on the PATH,
//! as editors do for their `code`-like commands. `dbr` ships next to the
//! app; this makes a terminal find it:
//!
//! - macOS: a link `/usr/local/bin/dbr` → the bundle's `dbr` (on the PATH by
//!   default; the system asks for an administrator's password when the
//!   folder isn't writable).
//! - Windows: the install folder added to the user's PATH (new terminals see it).
//! - Linux: the `.deb`/`.rpm` already install `/usr/bin/dbr`; the AppImage
//!   gets a script `~/.local/bin/dbr` that starts it in `query` mode (its
//!   files are mounted at a new place each run, so a link wouldn't last).

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Where `dbr` stands for this app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct DbrCommand {
    /// A terminal finds this app's `dbr` (a new terminal, on Windows).
    pub installed: bool,
    /// Where a terminal finds it, or would once installed (the link, the folder on PATH, the script).
    pub location: Option<String>,
    /// The program `dbr` runs.
    pub program: Option<String>,
    /// Another `dbr` a terminal finds first (an older install, another app).
    pub other: Option<String>,
    /// Install can be pressed (false: installed by the package, or no `dbr` next to the app).
    pub can_install: bool,
    /// What to know, when something is: why it can't be installed, a folder not on the PATH.
    pub note: Option<String>,
}

/// The `dbr` next to this program (`dbr.exe` on Windows), following links.
fn program() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let p = exe.parent()?.join(if cfg!(windows) { "dbr.exe" } else { "dbr" });
    p.is_file().then_some(p)
}

/// The first `name` on `path` (a PATH value).
fn on_path(path: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Whether `a` and `b` are the same file once links are followed.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Where `dbr` stands now.
pub fn status() -> DbrCommand {
    imp::status()
}

/// Put `dbr` on the PATH; where it stands then.
pub fn install() -> Result<DbrCommand, String> {
    imp::install()?;
    Ok(status())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    const LINK: &str = "/usr/local/bin/dbr";

    pub fn status() -> DbrCommand {
        let Some(program) = program() else {
            return DbrCommand {
                installed: false,
                location: None,
                program: None,
                other: None,
                can_install: false,
                note: Some("dbr isn't next to this DataBrain (a build without it)".into()),
            };
        };
        let link = Path::new(LINK);
        let installed = same_file(link, &program);
        // The PATH a login shell gets (/etc/paths): the app's own may be shorter.
        let path = std::env::var_os("PATH").unwrap_or_default();
        let first = on_path(&path, "dbr").filter(|p| !same_file(p, &program) && p != link);
        let other = match (installed, std::fs::symlink_metadata(link)) {
            // A link to another DataBrain (moved, or a second copy).
            (false, Ok(_)) => Some(std::fs::read_link(link).map(|t| format!("{LINK} → {}", t.display())).unwrap_or_else(|_| LINK.into())),
            _ => first.map(|p| p.display().to_string()),
        };
        DbrCommand { installed, location: Some(LINK.into()), program: Some(program.display().to_string()), other, can_install: !installed, note: None }
    }

    pub fn install() -> Result<(), String> {
        let program = program().ok_or("dbr isn't next to this DataBrain")?;
        let link = Path::new(LINK);
        // Only a link is replaced: a file there is someone else's dbr.
        if let Ok(m) = std::fs::symlink_metadata(link)
            && !m.file_type().is_symlink()
        {
            return Err(format!("{LINK} is a file, not a link to DataBrain: remove it first if you no longer need it"));
        }
        let tmp = PathBuf::from(format!("{LINK}.databrain-{}", std::process::id()));
        let direct = std::os::unix::fs::symlink(&program, &tmp).and_then(|()| std::fs::rename(&tmp, link));
        if direct.is_ok() {
            return Ok(());
        }
        let _ = std::fs::remove_file(&tmp);
        // Not writable (no Homebrew, or its folder is root's): as an administrator.
        let script = format!("mkdir -p /usr/local/bin && ln -sfn {} {LINK}", sh_quote(&program.to_string_lossy()));
        let apple = format!("do shell script \"{}\" with administrator privileges", apple_quote(&script));
        let out = std::process::Command::new("/usr/bin/osascript").arg("-e").arg(apple).output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            // -128: the password dialog was cancelled.
            Err(if err.contains("-128") { "Not installed: the administrator password was cancelled".into() } else { format!("Not installed: {}", err.trim()) })
        }
    }

    /// `s` as one shell word.
    pub(super) fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    /// `s` inside an AppleScript string literal.
    pub(super) fn apple_quote(s: &str) -> String {
        s.replace('\\', r"\\").replace('"', "\\\"")
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    /// `~/.local/bin/dbr`, where the AppImage's script and a link for other builds go.
    fn user_bin() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".local/bin"))
    }

    pub fn status() -> DbrCommand {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let found = on_path(&path, "dbr");
        // The .deb / .rpm: /usr/bin/dbr comes with the package.
        let packaged = Path::new("/usr/bin/dbr");
        if let Some(p) = program()
            && same_file(packaged, &p)
        {
            return DbrCommand {
                installed: true,
                location: Some("/usr/bin/dbr".into()),
                program: Some(p.display().to_string()),
                other: None,
                can_install: false,
                note: Some("Installed with the DataBrain package".into()),
            };
        }
        let Some(dir) = user_bin() else {
            return DbrCommand { installed: false, location: None, program: None, other: None, can_install: false, note: Some("No home folder".into()) };
        };
        let script = dir.join("dbr");
        let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
        let program = appimage.clone().or_else(program);
        let installed = match (&appimage, &program) {
            (Some(a), _) => std::fs::read_to_string(&script).is_ok_and(|s| s == wrapper(a)),
            (None, Some(p)) => same_file(&script, p),
            _ => false,
        };
        let other = found.filter(|f| f != &script).map(|f| f.display().to_string());
        let dir_on_path = std::env::split_paths(&path).any(|d| d == dir);
        DbrCommand {
            installed,
            location: Some(script.display().to_string()),
            program: program.map(|p| p.display().to_string()),
            other,
            can_install: !installed,
            note: (!dir_on_path).then(|| format!("{} isn't on your PATH: add it in your shell's profile", dir.display())),
        }
    }

    /// The script that runs the AppImage at `appimage` as `dbr`.
    pub(super) fn wrapper(appimage: &Path) -> String {
        format!(
            "#!/bin/sh\n# dbr: DataBrain's command line (written by DataBrain)\nexec '{}' query \"$@\"\n",
            appimage.to_string_lossy().replace('\'', r"'\''")
        )
    }

    pub fn install() -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let dir = user_bin().ok_or("no home folder")?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let target = dir.join("dbr");
        if let Ok(m) = std::fs::symlink_metadata(&target) {
            let ours = m.file_type().is_symlink() || std::fs::read_to_string(&target).is_ok_and(|s| s.contains("written by DataBrain"));
            if !ours {
                return Err(format!("{} is someone else's dbr: remove it first if you no longer need it", target.display()));
            }
        }
        let tmp = dir.join(format!(".dbr.databrain-{}", std::process::id()));
        let made = match std::env::var_os("APPIMAGE") {
            Some(a) => std::fs::write(&tmp, wrapper(Path::new(&a))).and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))),
            None => std::os::unix::fs::symlink(program().ok_or("dbr isn't next to this DataBrain")?, &tmp),
        };
        made.and_then(|()| std::fs::rename(&tmp, &target)).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::process::Command;

    /// The user's PATH as saved (what new terminals get), via PowerShell.
    fn user_path() -> Option<String> {
        let out = powershell("[Environment]::GetEnvironmentVariable('Path','User')", None).ok()?;
        Some(out.trim().to_string())
    }

    fn powershell(script: &str, dir: Option<&Path>) -> Result<String, String> {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        // The folder goes in as an environment variable, never in the script's text.
        if let Some(d) = dir {
            cmd.env("DATABRAIN_DBR_DIR", d);
        }
        // CREATE_NO_WINDOW: no console flashing up.
        cmd.creation_flags(0x0800_0000);
        let out = cmd.output().map_err(|e| e.to_string())?;
        if out.status.success() { Ok(String::from_utf8_lossy(&out.stdout).into_owned()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
    }

    fn has_dir(path: &str, dir: &Path) -> bool {
        let want = dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
        path.split(';').any(|p| p.trim().trim_end_matches('\\').to_lowercase() == want)
    }

    pub fn status() -> DbrCommand {
        let Some(program) = program() else {
            return DbrCommand {
                installed: false,
                location: None,
                program: None,
                other: None,
                can_install: false,
                note: Some("dbr isn't next to this DataBrain".into()),
            };
        };
        let dir = program.parent().map(Path::to_path_buf).unwrap_or_default();
        let saved = user_path().unwrap_or_default();
        let installed = has_dir(&saved, &dir) || std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir));
        let other = std::env::var_os("PATH").and_then(|p| on_path(&p, "dbr.exe")).filter(|p| !same_file(p, &program)).map(|p| p.display().to_string());
        DbrCommand {
            installed,
            location: Some(dir.display().to_string()),
            program: Some(program.display().to_string()),
            other,
            can_install: !installed,
            note: installed.then(|| "Terminals opened before it was installed don't see it: open a new one".into()),
        }
    }

    pub fn install() -> Result<(), String> {
        let program = program().ok_or("dbr isn't next to this DataBrain")?;
        let dir = program.parent().ok_or("no install folder")?;
        // Appended to the user's own PATH (not the machine's: no administrator needed).
        powershell(
            "$p = [Environment]::GetEnvironmentVariable('Path','User'); $d = $env:DATABRAIN_DBR_DIR; \
             if (-not (($p -split ';') | Where-Object { $_.TrimEnd('\\') -ieq $d.TrimEnd('\\') })) { \
               $n = if ([string]::IsNullOrEmpty($p)) { $d } else { $p.TrimEnd(';') + ';' + $d }; \
               [Environment]::SetEnvironmentVariable('Path', $n, 'User') }",
            Some(dir),
        )
        .map(|_| ())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod imp {
    use super::*;
    pub fn status() -> DbrCommand {
        DbrCommand { installed: false, location: None, program: program().map(|p| p.display().to_string()), other: None, can_install: false, note: None }
    }
    pub fn install() -> Result<(), String> {
        Err("not on this system".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_lookup_and_same_file() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(b.path().join("dbr"), "x").unwrap();
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        assert_eq!(on_path(&path, "dbr"), Some(b.path().join("dbr")));
        assert_eq!(on_path(&path, "nope"), None);
        assert!(same_file(&b.path().join("dbr"), &b.path().join(".").join("dbr")));
        assert!(!same_file(&b.path().join("dbr"), &a.path().join("dbr")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_admin_script_quotes_the_path() {
        let s = format!("ln -sfn {} /usr/local/bin/dbr", imp::sh_quote("/Applications/My \"Data\" Brain's.app/Contents/MacOS/dbr"));
        assert_eq!(s, r#"ln -sfn '/Applications/My "Data" Brain'\''s.app/Contents/MacOS/dbr' /usr/local/bin/dbr"#);
        assert_eq!(imp::apple_quote(&s), r#"ln -sfn '/Applications/My \"Data\" Brain'\\''s.app/Contents/MacOS/dbr' /usr/local/bin/dbr"#);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_appimage_script_starts_it_in_query_mode() {
        let w = imp::wrapper(Path::new("/home/me/Apps/Data Brain's.AppImage"));
        assert!(w.contains(r"exec '/home/me/Apps/Data Brain'\''s.AppImage' query "), "{w}");
        assert!(w.contains("written by DataBrain"));
    }
}
