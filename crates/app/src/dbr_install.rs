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
    let exe = std::fs::canonicalize(&exe).map(plain).unwrap_or(exe);
    let p = exe.parent()?.join(if cfg!(windows) { "dbr.exe" } else { "dbr" });
    p.is_file().then_some(p)
}

/// A canonical Windows path without its `\\?\` prefix (`\\?\C:\…` → `C:\…`):
/// a PATH entry with it isn't searched reliably. UNC paths keep theirs.
fn plain(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

/// Whether the link at `link` is a DataBrain `dbr` (this one, an older or
/// moved copy): its target is named `dbr` and sits next to a `databrain`
/// program, or in an app bundle. Another tool's `dbr` link is left alone.
#[cfg(unix)]
fn our_link(link: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else { return false };
    let target = if target.is_relative() { link.parent().map(|d| d.join(&target)).unwrap_or(target) } else { target };
    target.file_name().is_some_and(|n| n == "dbr")
        && (target.parent().is_some_and(|d| d.join("databrain").is_file() || d.join("DataBrain").is_file())
            || target.to_string_lossy().contains(".app/Contents/MacOS/"))
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
        let path = terminal_path();
        let first = on_path(&path, "dbr").filter(|p| !same_file(p, &program) && p != link);
        let other = match (installed, std::fs::symlink_metadata(link)) {
            // Something else at the link's place (a moved DataBrain, a file).
            (false, Ok(_)) => Some(std::fs::read_link(link).map(|t| format!("{LINK} → {}", t.display())).unwrap_or_else(|_| LINK.into())),
            _ => first.map(|p| p.display().to_string()),
        };
        let temporary = temporary_copy(&program);
        DbrCommand {
            installed,
            location: Some(LINK.into()),
            program: Some(program.display().to_string()),
            other,
            can_install: !installed && temporary.is_none(),
            note: temporary,
        }
    }

    /// Why a link to `program` wouldn't last: DataBrain runs from the disk
    /// image, or from the copy macOS makes of an app not moved yet.
    fn temporary_copy(program: &Path) -> Option<String> {
        let s = program.to_string_lossy();
        (s.starts_with("/Volumes/") || s.contains("/AppTranslocation/"))
            .then(|| "DataBrain runs from a temporary place (the disk image, or a copy macOS made): move it to Applications first, then install".into())
    }

    /// The PATH a terminal gets: Homebrew's folder, /etc/paths and
    /// /etc/paths.d (the app's own PATH, started from the Finder, is shorter).
    fn terminal_path() -> std::ffi::OsString {
        let mut dirs: Vec<PathBuf> = vec!["/opt/homebrew/bin".into()];
        let mut add = |text: String| dirs.extend(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from));
        if let Ok(t) = std::fs::read_to_string("/etc/paths") {
            add(t);
        }
        if let Ok(rd) = std::fs::read_dir("/etc/paths.d") {
            let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            files.sort();
            for f in files {
                if let Ok(t) = std::fs::read_to_string(f) {
                    add(t);
                }
            }
        }
        if let Some(p) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&p));
        }
        std::env::join_paths(dirs).unwrap_or_default()
    }

    pub fn install() -> Result<(), String> {
        let program = program().ok_or("dbr isn't next to this DataBrain")?;
        if let Some(why) = temporary_copy(&program) {
            return Err(why);
        }
        let link = Path::new(LINK);
        // Only a DataBrain link is replaced: anything else there is someone else's dbr.
        if std::fs::symlink_metadata(link).is_ok() && !our_link(link) {
            return Err(format!("{LINK} is another program's dbr: remove it first if you no longer need it"));
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
        // The AppImage's own files (its folder may be on the PATH it runs with) aren't another dbr.
        let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
        let found = on_path(&path, "dbr").filter(|f| appdir.as_ref().is_none_or(|a| !f.starts_with(a)));
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
        let placed = match (&appimage, &program) {
            (Some(a), _) => std::fs::read_to_string(&script).is_ok_and(|s| s == wrapper(a)),
            (None, Some(p)) => same_file(&script, p),
            _ => false,
        };
        let other = found.filter(|f| f != &script).map(|f| f.display().to_string());
        let dir_on_path = std::env::split_paths(&path).any(|d| d == dir);
        DbrCommand {
            // A terminal finds it only with the folder on its PATH.
            installed: placed && dir_on_path,
            location: Some(script.display().to_string()),
            program: program.map(|p| p.display().to_string()),
            other,
            can_install: !placed,
            note: (!dir_on_path).then(|| {
                format!(
                    "{} isn't on the PATH yet: log out and back in (most systems add it at login once it exists), or add it in your shell's profile",
                    dir.display()
                )
            }),
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
        if std::fs::symlink_metadata(&target).is_ok() {
            let ours = our_link(&target) || std::fs::read_to_string(&target).is_ok_and(|s| s.contains("written by DataBrain"));
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

    /// The user's PATH as saved (what new terminals get), its `%VAR%`
    /// entries expanded, via PowerShell.
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
        let env_path = std::env::var_os("PATH").map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let installed = has_dir(&saved, &dir) || has_dir(&env_path, &dir);
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
        // Appended to the user's own PATH (not the machine's: no administrator
        // needed), read and written unexpanded and as an expandable string,
        // so its `%VAR%` entries stay as they are. Setting and clearing a
        // variable through .NET then tells running programs (Explorer) that
        // the environment changed, so new terminals see it.
        powershell(
            concat!(
                r"$ErrorActionPreference = 'Stop'; ",
                r"$p = (Get-Item -Path 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames'); $d = $env:DATABRAIN_DBR_DIR; ",
                r"if (-not (($p -split ';') | Where-Object { [Environment]::ExpandEnvironmentVariables($_).Trim().TrimEnd('\') -ieq $d.TrimEnd('\') })) { ",
                r"$n = if ([string]::IsNullOrEmpty($p)) { $d } else { $p.TrimEnd(';') + ';' + $d }; ",
                r"New-ItemProperty -Path 'HKCU:\Environment' -Name 'Path' -PropertyType ExpandString -Value $n -Force | Out-Null; ",
                r"[Environment]::SetEnvironmentVariable('DATABRAIN_PATH_CHANGED', '1', 'User'); ",
                r"[Environment]::SetEnvironmentVariable('DATABRAIN_PATH_CHANGED', $null, 'User') }",
            ),
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

    #[test]
    fn a_windows_path_loses_its_long_prefix() {
        assert_eq!(plain(PathBuf::from(r"\\?\C:\Program Files\DataBrain\dbr.exe")), PathBuf::from(r"C:\Program Files\DataBrain\dbr.exe"));
        assert_eq!(plain(PathBuf::from(r"\\?\UNC\srv\share\dbr.exe")), PathBuf::from(r"\\?\UNC\srv\share\dbr.exe"));
        assert_eq!(plain(PathBuf::from("/usr/bin/dbr")), PathBuf::from("/usr/bin/dbr"));
    }

    #[cfg(unix)]
    #[test]
    fn only_a_databrain_link_counts_as_ours() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("app");
        let tool = d.path().join("tool");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&tool).unwrap();
        std::fs::write(app.join("databrain"), "").unwrap();
        std::fs::write(app.join("dbr"), "").unwrap();
        std::fs::write(tool.join("dbr"), "").unwrap();
        let ours = d.path().join("ours");
        let theirs = d.path().join("theirs");
        let moved = d.path().join("moved");
        std::os::unix::fs::symlink(app.join("dbr"), &ours).unwrap();
        std::os::unix::fs::symlink(tool.join("dbr"), &theirs).unwrap();
        std::os::unix::fs::symlink("/Applications/Old DataBrain.app/Contents/MacOS/dbr", &moved).unwrap();
        assert!(our_link(&ours));
        assert!(!our_link(&theirs), "another tool's dbr");
        assert!(our_link(&moved), "a DataBrain moved or deleted since");
        assert!(!our_link(&app.join("dbr")), "a file isn't a link");
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
