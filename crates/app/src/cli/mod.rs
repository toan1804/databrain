//! The command-line modes of the one `databrain` program. The app, the MCP
//! server and the jobs runner share almost all their code, so they ship as
//! one executable instead of three copies of it: `databrain mcp` and
//! `databrain jobs …`, also reached through the small `databrain-mcp` and
//! `databrain-jobs` launchers next to it (what MCP clients and the OS
//! scheduler are set up to start).

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

pub mod jobs;
pub mod mcp;

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    App,
    Mcp(Vec<String>),
    Jobs(Vec<String>),
}

/// The mode `args` ask for (the program's arguments, its name first): a
/// subcommand, or a launcher's name when started through a link to this
/// program. Anything else opens the app (macOS passes arguments of its own).
pub fn mode(args: Vec<OsString>) -> Mode {
    let mut it = args.into_iter();
    let name = it.next().and_then(|a| Path::new(&a).file_stem().map(|s| s.to_string_lossy().to_lowercase())).unwrap_or_default();
    let rest: Vec<String> = it.map(|a| a.to_string_lossy().into_owned()).collect();
    match (name.as_str(), rest.first().map(String::as_str)) {
        ("databrain-mcp", _) => Mode::Mcp(rest),
        ("databrain-jobs", _) => Mode::Jobs(rest),
        (_, Some("mcp")) => Mode::Mcp(rest[1..].to_vec()),
        (_, Some("jobs")) => Mode::Jobs(rest[1..].to_vec()),
        _ => Mode::App,
    }
}

/// The runtime of a command-line mode. Multi-thread, like the app's
/// (Tauri's): the query engine's `blocking` hands a worker thread over
/// for workspace and file writes only on a multi-thread runtime; on a
/// current-thread one they'd stall every other task.
pub(crate) fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()
}

/// Run a command-line mode; `None` when the app should open.
pub fn dispatch(args: Vec<OsString>) -> Option<ExitCode> {
    match mode(args) {
        Mode::App => None,
        Mode::Mcp(rest) => Some(mcp::main(rest)),
        Mode::Jobs(rest) => Some(jobs::main(rest)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(args: &[&str]) -> Mode {
        mode(args.iter().map(OsString::from).collect())
    }
    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn modes_by_subcommand_or_launcher_name() {
        assert_eq!(m(&["/Applications/DataBrain.app/Contents/MacOS/databrain"]), Mode::App);
        assert_eq!(m(&["databrain", "mcp"]), Mode::Mcp(vec![]));
        assert_eq!(m(&["C:\\Program Files\\DataBrain\\databrain.exe", "jobs", "run", "Daily load"]), Mode::Jobs(v(&["run", "Daily load"])));
        assert_eq!(m(&["/usr/bin/databrain-mcp"]), Mode::Mcp(vec![]));
        assert_eq!(m(&["DataBrain-Jobs.exe", "list"]), Mode::Jobs(v(&["list"])));
        // macOS passes arguments of its own to apps; they open the app.
        assert_eq!(m(&["databrain", "-psn_0_12345"]), Mode::App);
        assert_eq!(m(&[]), Mode::App);
    }
}
