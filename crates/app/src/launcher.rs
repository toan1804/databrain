// Shared by the `databrain-mcp` and `databrain-jobs` launchers: start the
// `databrain` program next to them in that mode. They use std only, so
// they stay a few hundred KB instead of a copy of DataBrain each.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

/// `databrain` next to this launcher (`DataBrain` too, in case a bundle
/// names it after the product). A link to the launcher (`/usr/local/bin/
/// databrain-jobs` → the app bundle) is followed to where it really is:
/// macOS gives the link's own path as the current executable.
fn program() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe.parent()?.to_path_buf();
    let ext = if cfg!(windows) { ".exe" } else { "" };
    ["databrain", "DataBrain"].iter().map(|n| dir.join(format!("{n}{ext}"))).find(|p| p.is_file())
}

/// Run `databrain <mode> <this launcher's arguments>` in place of this
/// process (Unix), or as a child whose exit code becomes ours (Windows).
pub fn launch(mode: &str, name: &str) -> ExitCode {
    let Some(exe) = program() else {
        eprintln!("{name}: the databrain program isn't next to {name}; reinstall DataBrain");
        return ExitCode::from(127);
    };
    let mut cmd = Command::new(&exe);
    cmd.arg(mode).args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Same process: stdin/stdout (MCP), signals and exit code pass straight through.
        let e = cmd.exec();
        eprintln!("{name}: couldn't start {}: {e}", exe.display());
        ExitCode::from(127)
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(s) => ExitCode::from(u8::try_from(s.code().unwrap_or(1)).unwrap_or(1)),
        Err(e) => {
            eprintln!("{name}: couldn't start {}: {e}", exe.display());
            ExitCode::from(127)
        }
    }
}
