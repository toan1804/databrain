//! Scheduled jobs while DataBrain is closed (off by default).
//!
//! By default schedules run while the app is open, as before. With the
//! `background_jobs` setting on, the OS starts `databrain-jobs tick` every
//! minute (a launchd agent on macOS, a systemd user timer on Linux, a Task
//! Scheduler task on Windows): it runs the jobs that are due and exits.
//! A job runs in one process at a time (a lease in the workspace), so the
//! app and the runner never run the same job twice. Failures of background
//! runs show a desktop notification.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use databrain_auth::SwitchableStore;
use databrain_query_engine::{EngineError, EventSink, JobEvent, Result};
use databrain_workspace::Workspace;
use serde::Serialize;

use crate::api::{self, AppState};

/// Setting: run scheduled jobs when the app is closed (default false).
pub const BACKGROUND_SETTING: &str = "background_jobs";
/// Setting written by the runner after each pass (ms), shown in Settings.
pub const LAST_TICK_SETTING: &str = "background_jobs_last_tick";

const LABEL: &str = "dev.databrain.jobs";
const WINDOWS_TASK: &str = "DataBrain scheduled jobs";

/// The app's data folder (same as the desktop app's; `DATABRAIN_DATA_DIR` overrides).
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DATABRAIN_DATA_DIR") {
        return d.into();
    }
    if let Some(ws) = std::env::var_os("DATABRAIN_WORKSPACE") {
        if let Some(p) = Path::new(&ws).parent() {
            return p.to_path_buf();
        }
    }
    dirs::data_dir().unwrap_or_else(|| ".".into()).join("dev.databrain.app")
}

struct Quiet;
impl EventSink for Quiet {
    fn emit(&self, _: JobEvent) {}
}

/// No window: events go nowhere; links can't be opened.
struct NoUi;
impl crate::ai_api::UiBridge for NoUi {
    fn emit(&self, _: &str, _: serde_json::Value) {}
    fn open_url(&self, _: &str) -> std::result::Result<(), String> {
        Err("no window to open links (background runner)".into())
    }
}

/// The app's state without a window, as `kind` (`cli`): same workspace,
/// passwords, DuckDB extensions and Oracle driver as the desktop app.
pub fn headless_state(dir: &Path) -> Result<AppState> {
    let workspace = Arc::new(Workspace::open(dir.join("workspace.db")).map_err(|e| EngineError::new("internal", format!("cannot open the workspace in {}: {e}", dir.display())))?);
    let store = Arc::new(SwitchableStore::new(api::saved_store_kind(&workspace), dir));
    let mut state = AppState::new(workspace, store.clone(), Arc::new(Quiet), Arc::new(NoUi));
    state.jobs = Arc::new(crate::jobs::JobsState::new("cli"));
    state.set_credential_store(store, dir.to_path_buf());
    state.set_snapshot_dir(dir.join("outputs"));
    api::apply_oracle_client_setting(&state);
    api::setup_oracle_agent(&state, dir);
    #[cfg(feature = "duckdb-engine")]
    let _ = api::load_duckdb_engine_from_settings(&state.workspace);
    // Extensions the app copied from its bundle.
    #[cfg(feature = "duckdb")]
    databrain_connector_duckdb::set_extension_dir(Some(dir.join("duckdb_extensions").to_string_lossy().into_owned()));
    Ok(state)
}

pub fn enabled(ws: &Workspace) -> bool {
    ws.get_setting(BACKGROUND_SETTING).ok().flatten().and_then(|v| v.as_bool()).unwrap_or(false)
}

/// The runner next to this executable (`databrain-jobs`, `.exe` on Windows).
pub fn runner_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| EngineError::new("internal", e.to_string()))?;
    let p = exe.parent().map(|d| d.join(if cfg!(windows) { "databrain-jobs.exe" } else { "databrain-jobs" })).unwrap_or_default();
    if !p.is_file() {
        return Err(EngineError::new("not_found", format!("The background runner is missing next to the app ({}). Reinstall DataBrain.", p.display())));
    }
    Ok(p)
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundStatus {
    pub enabled: bool,
    /// The OS entry that starts the runner is in place.
    pub installed: bool,
    /// launchd agent / systemd user timer / Task Scheduler task.
    pub mechanism: String,
    /// Where the OS entry is (file or task name).
    pub location: String,
    /// Last pass of the runner (ms), if any.
    pub last_tick: Option<i64>,
    pub log_file: String,
}

fn log_file(dir: &Path) -> PathBuf {
    dir.join("logs").join("databrain-jobs.log")
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| EngineError::new("internal", "no home folder"))
}

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(cmd).args(args).output().map_err(|e| EngineError::new("internal", format!("{cmd}: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(EngineError::new("internal", format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())))
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// launchd agent: runs `databrain-jobs tick` every minute while the user is logged in.
pub fn launchd_plist(runner: &Path, log: &Path) -> String {
    let (r, l) = (xml_escape(&runner.to_string_lossy()), xml_escape(&log.to_string_lossy()));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{r}</string><string>tick</string></array>
  <key>StartInterval</key><integer>60</integer>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{l}</string>
  <key>StandardErrorPath</key><string>{l}</string>
</dict>
</plist>
"#
    )
}

/// systemd user units: a oneshot service and a timer every minute.
pub fn systemd_units(runner: &Path) -> (String, String) {
    let r = runner.to_string_lossy().replace('"', "\\\"");
    (
        format!("[Unit]\nDescription=DataBrain scheduled jobs\n\n[Service]\nType=oneshot\nExecStart=\"{r}\" tick\n"),
        "[Unit]\nDescription=Run DataBrain scheduled jobs every minute\n\n[Timer]\nOnBootSec=1min\nOnUnitActiveSec=1min\nAccuracySec=10s\nUnit=databrain-jobs.service\n\n[Install]\nWantedBy=timers.target\n".to_string(),
    )
}

fn entry_location() -> Result<(String, PathBuf)> {
    if cfg!(target_os = "macos") {
        Ok(("launchd agent".into(), home()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))))
    } else if cfg!(windows) {
        Ok(("Task Scheduler task".into(), PathBuf::from(WINDOWS_TASK)))
    } else {
        Ok(("systemd user timer".into(), dirs::config_dir().unwrap_or(home()?.join(".config")).join("systemd/user/databrain-jobs.timer")))
    }
}

fn installed() -> bool {
    match entry_location() {
        Ok((_, p)) if cfg!(windows) => run("schtasks", &["/Query", "/TN", &p.to_string_lossy()]).is_ok(),
        Ok((_, p)) => p.is_file(),
        Err(_) => false,
    }
}

pub fn status(state: &AppState, dir: &Path) -> Result<BackgroundStatus> {
    let (mechanism, loc) = entry_location()?;
    Ok(BackgroundStatus {
        enabled: enabled(&state.workspace),
        installed: installed(),
        mechanism,
        location: loc.to_string_lossy().into_owned(),
        last_tick: state.workspace.get_setting(LAST_TICK_SETTING).ok().flatten().and_then(|v| v.as_i64()),
        log_file: log_file(dir).to_string_lossy().into_owned(),
    })
}

/// Turn the background runner on (install the OS entry) or off (remove it).
pub fn set_enabled(state: &AppState, dir: &Path, on: bool) -> Result<BackgroundStatus> {
    let (_, loc) = entry_location()?;
    if on {
        let runner = runner_path()?;
        let log = log_file(dir);
        if let Some(p) = log.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if cfg!(target_os = "macos") {
            if let Some(p) = loc.parent() {
                std::fs::create_dir_all(p).map_err(|e| EngineError::new("io", e.to_string()))?;
            }
            std::fs::write(&loc, launchd_plist(&runner, &log)).map_err(|e| EngineError::new("io", e.to_string()))?;
            let uid = run("id", &["-u"])?.trim().to_string();
            // Reload when it was loaded already (a new app path).
            let _ = run("launchctl", &["bootout", &format!("gui/{uid}/{LABEL}")]);
            run("launchctl", &["bootstrap", &format!("gui/{uid}"), &loc.to_string_lossy()])?;
        } else if cfg!(windows) {
            let tr = format!("\"{}\" tick", runner.display());
            run("schtasks", &["/Create", "/F", "/SC", "MINUTE", "/MO", "1", "/TN", WINDOWS_TASK, "/TR", &tr])?;
        } else {
            let dirp = loc.parent().map(Path::to_path_buf).unwrap_or_default();
            std::fs::create_dir_all(&dirp).map_err(|e| EngineError::new("io", e.to_string()))?;
            let (service, timer) = systemd_units(&runner);
            std::fs::write(dirp.join("databrain-jobs.service"), service).map_err(|e| EngineError::new("io", e.to_string()))?;
            std::fs::write(&loc, timer).map_err(|e| EngineError::new("io", e.to_string()))?;
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", "--now", "databrain-jobs.timer"])?;
        }
    } else if cfg!(target_os = "macos") {
        if let Ok(uid) = run("id", &["-u"]) {
            let _ = run("launchctl", &["bootout", &format!("gui/{}/{LABEL}", uid.trim())]);
        }
        let _ = std::fs::remove_file(&loc);
    } else if cfg!(windows) {
        let _ = run("schtasks", &["/Delete", "/F", "/TN", WINDOWS_TASK]);
    } else {
        let _ = run("systemctl", &["--user", "disable", "--now", "databrain-jobs.timer"]);
        let _ = std::fs::remove_file(&loc);
        if let Some(p) = loc.parent() {
            let _ = std::fs::remove_file(p.join("databrain-jobs.service"));
        }
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    state.workspace.set_setting(BACKGROUND_SETTING, &serde_json::json!(on))?;
    status(state, dir)
}

/// A desktop notification (best effort; nothing when the OS has no way).
pub fn notify(title: &str, body: &str) {
    let _ = if cfg!(target_os = "macos") {
        let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        std::process::Command::new("osascript").args(["-e", &format!("display notification \"{}\" with title \"{}\"", q(body), q(title))]).output()
    } else if cfg!(windows) {
        let q = |s: &str| s.replace('\'', "''");
        let ps = format!(
            "Add-Type -AssemblyName System.Windows.Forms; $n = New-Object System.Windows.Forms.NotifyIcon; $n.Icon = [System.Drawing.SystemIcons]::Information; $n.Visible = $true; $n.ShowBalloonTip(10000, '{}', '{}', 'Error'); Start-Sleep -Seconds 10; $n.Dispose()",
            q(title),
            q(body)
        );
        std::process::Command::new("powershell").args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps]).output()
    } else {
        std::process::Command::new("notify-send").args(["--app-name=DataBrain", title, body]).output()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_entries_run_the_runner_every_minute() {
        let p = launchd_plist(Path::new("/Applications/DataBrain.app/Contents/MacOS/databrain-jobs"), Path::new("/tmp/a&b.log"));
        assert!(p.contains("<string>/Applications/DataBrain.app/Contents/MacOS/databrain-jobs</string><string>tick</string>"));
        assert!(p.contains("<integer>60</integer>") && p.contains("/tmp/a&amp;b.log"));
        let (s, t) = systemd_units(Path::new("/opt/Data Brain/databrain-jobs"));
        assert!(s.contains("ExecStart=\"/opt/Data Brain/databrain-jobs\" tick"));
        assert!(t.contains("OnUnitActiveSec=1min"));
    }
}
