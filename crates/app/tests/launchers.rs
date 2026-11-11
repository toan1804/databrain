//! The `databrain-mcp` and `databrain-jobs` launchers start the one
//! `databrain` program in that mode: what MCP clients and the OS scheduler
//! run keeps working, with stdin/stdout and exit codes passed through.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn databrain_jobs_runs_the_jobs_mode() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_databrain-jobs"));
        c.env("DATABRAIN_DATA_DIR", dir.path());
        c
    };
    let list = jobs().arg("list").output().unwrap();
    assert!(list.status.success(), "{}", String::from_utf8_lossy(&list.stderr));

    let missing = jobs().args(["run", "no such job"]).output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no job named"), "{}", String::from_utf8_lossy(&missing.stderr));

    let usage = jobs().output().unwrap();
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&usage.stderr).contains("usage: databrain-jobs"));

    // The program itself takes the same command.
    let direct = Command::new(env!("CARGO_BIN_EXE_databrain")).args(["jobs", "list"]).env("DATABRAIN_DATA_DIR", dir.path()).output().unwrap();
    assert!(direct.status.success(), "{}", String::from_utf8_lossy(&direct.stderr));
}

#[test]
fn databrain_mcp_serves_mcp_on_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_databrain-mcp"))
        .env("DATABRAIN_WORKSPACE", dir.path().join("workspace.db"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
    // Closing stdin after the request ends the server.
    child.stdin.take().unwrap().write_all(format!("{init}\n").as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(r#""id":1"#) && text.contains(r#""name":"databrain""#), "{text}");
}

/// A link to a launcher elsewhere (`ln -s …/databrain-jobs /usr/local/bin/`)
/// still finds the program next to the real launcher.
#[cfg(unix)]
#[test]
fn launchers_work_through_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("databrain-jobs");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_databrain-jobs"), &link).unwrap();
    let out = Command::new(&link).arg("list").env("DATABRAIN_DATA_DIR", dir.path()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// The launchers are small: they don't carry a copy of DataBrain.
#[test]
fn launchers_are_small() {
    for exe in [env!("CARGO_BIN_EXE_databrain-mcp"), env!("CARGO_BIN_EXE_databrain-jobs")] {
        let size = std::fs::metadata(exe).unwrap().len();
        assert!(size < 5 * 1024 * 1024, "{exe}: {size} bytes");
    }
}
