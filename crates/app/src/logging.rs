//! Diagnostic log: `<app data>/logs/<process>.log.<date>` (one file a day,
//! the last [`KEPT_FILES`] kept), and stderr for the command-line tools.
//!
//! The level comes from `DATABRAIN_LOG` (`tracing` filter syntax, e.g.
//! `debug` or `databrain_query_engine=debug`); default `info`. Logs carry
//! what happened (connections, runs, jobs, AI tool decisions, errors), not
//! data: never query text, result values or secrets.

use std::path::Path;
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Daily files kept per process.
pub const KEPT_FILES: usize = 7;

/// Keeps the file writer flushing until the process ends.
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Folder of the log files under the app data folder.
pub fn dir(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("logs")
}

/// Start logging for this process (`name`: `databrain`, `databrain-mcp`,
/// `databrain-jobs`). `data_dir`: write files there; `stderr`: also to
/// stderr (never stdout: the MCP server speaks its protocol there). Panics
/// are logged too. Calling it again does nothing.
pub fn init(name: &str, data_dir: Option<&Path>, stderr: bool) {
    let filter = || EnvFilter::try_from_env("DATABRAIN_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let file = data_dir.and_then(|d| {
        std::fs::create_dir_all(dir(d)).ok()?;
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(name)
            .filename_suffix("log")
            .max_log_files(KEPT_FILES)
            .build(dir(d))
            .ok()?;
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let _ = GUARD.set(guard);
        Some(fmt::layer().with_ansi(false).with_target(true).with_writer(writer).with_filter(filter()))
    });
    use std::io::IsTerminal;
    let color = std::io::stderr().is_terminal();
    let err = stderr.then(|| fmt::layer().with_target(false).with_ansi(color).with_writer(std::io::stderr).with_filter(filter()));
    if tracing_subscriber::registry().with(file).with(err).try_init().is_err() {
        return;
    }
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        tracing::error!(target: "panic", at, "{msg}");
        default(info);
    }));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, arch = std::env::consts::ARCH, "{name} started");
}
