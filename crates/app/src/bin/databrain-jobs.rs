//! `databrain-jobs`: runs DataBrain jobs without the app's window.
//!
//! ```text
//! databrain-jobs tick            run the scheduled jobs that are due, then exit
//!                                (what the OS starts every minute when
//!                                "Run scheduled jobs when DataBrain is closed" is on)
//! databrain-jobs run <job>       run a job now (name or id) and wait; exit code 1 if it failed
//! databrain-jobs list            jobs, schedules, next and last runs
//! ```
//!
//! It uses the same workspace, passwords and drivers as the app
//! (`DATABRAIN_DATA_DIR` points at another data folder). A job runs in one
//! process at a time, so it never runs a job the app is running.

// No console window every minute on Windows (the task runs `tick`); `run`
// and `list` attach to the terminal they were started from.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use databrain_app::background;
use databrain_app::jobs;

fn usage() -> ExitCode {
    eprintln!("usage: databrain-jobs tick [--force] | run <job name or id> | list");
    ExitCode::from(2)
}

fn when(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

#[tokio::main]
async fn main() -> ExitCode {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else { return usage() };
    let dir = background::data_dir();
    let state = match background::headless_state(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("databrain-jobs: {}", e.message);
            return ExitCode::from(2);
        }
    };
    let ctx = state.job_ctx();
    match cmd {
        "tick" => {
            // Off in the app's settings: nothing to do (an OS entry left behind).
            if !background::enabled(&state.workspace) && !args.iter().any(|a| a == "--force") {
                return ExitCode::SUCCESS;
            }
            let started = jobs::tick_as(&ctx, "background");
            let _ = state.workspace.set_setting(background::LAST_TICK_SETTING, &serde_json::json!(databrain_workspace::now_ms()));
            if started.is_empty() {
                return ExitCode::SUCCESS;
            }
            jobs::wait_for(&ctx, &started).await;
            let mut failed = false;
            for id in &started {
                let job = state.workspace.get_job(id).ok();
                let name = job.as_ref().map(|j| j.name.clone()).unwrap_or_else(|| id.clone());
                let run = state.workspace.job_runs(id, 1).ok().and_then(|r| r.into_iter().next());
                let status = run.as_ref().map(|r| r.status.clone()).unwrap_or_default();
                println!("{} {name}: {status}", when(databrain_workspace::now_ms()));
                if status == "error" {
                    failed = true;
                    let why = run.and_then(|r| r.error).unwrap_or_else(|| "see its runs in DataBrain".into());
                    background::notify("DataBrain: scheduled job failed", &format!("{name}: {why}"));
                }
            }
            if failed { ExitCode::from(1) } else { ExitCode::SUCCESS }
        }
        "run" => {
            let Some(which) = args.get(1) else { return usage() };
            let list = state.workspace.list_jobs().unwrap_or_default();
            let Some(job) = list.iter().find(|j| &j.id == which).or_else(|| list.iter().find(|j| j.name.eq_ignore_ascii_case(which))) else {
                eprintln!("databrain-jobs: no job named {which:?} (databrain-jobs list shows them)");
                return ExitCode::from(2);
            };
            println!("Running {}…", job.name);
            match jobs::run_and_wait(&ctx, &job.id, "cli").await {
                Ok(run) => {
                    for n in &run.nodes {
                        println!("  {:<24} {:<10} {}", n.name, n.summary.status, n.summary.rows.map(|r| format!("{r} rows")).or(n.summary.error.clone()).unwrap_or_default());
                    }
                    println!("{}: {}", job.name, run.status);
                    if run.status == "success" { ExitCode::SUCCESS } else { ExitCode::from(1) }
                }
                Err(e) => {
                    eprintln!("databrain-jobs: {}", e.message);
                    ExitCode::from(2)
                }
            }
        }
        "list" => {
            for j in state.workspace.list_jobs().unwrap_or_default() {
                let next = if j.schedule.enabled { j.next_run_at.map(when).unwrap_or_else(|| "—".into()) } else { "not scheduled".into() };
                let last = j.last_run.map(|r| format!("{} {}", r.status, when(r.started_at))).unwrap_or_else(|| "never ran".into());
                println!("{:<32} next: {:<18} last: {}", j.name, next, last);
            }
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}
