//! `databrain jobs` (also started as `databrain-jobs`): runs DataBrain jobs
//! without the app's window.
//!
//! ```text
//! databrain-jobs tick            run the scheduled jobs that are due, then exit
//!                                (what the OS starts every minute when
//!                                "Run scheduled jobs when DataBrain is closed" is on)
//! databrain-jobs run <job>       run a job now (name or id) and wait; exit code 1 if it failed
//! databrain-jobs list            jobs, schedules, next and last runs
//! databrain-jobs runs <job>      its last runs (--limit N, 5 by default), the latest's steps
//! databrain-jobs stop <job>      stop its run, wherever it runs (the app, the background
//!                                runner, another `run`), and wait for it (--no-wait)
//! ```
//!
//! `dbr jobs …` is the same.
//!
//! It uses the same workspace, passwords and drivers as the app
//! (`DATABRAIN_DATA_DIR` points at another data folder). A job runs in one
//! process at a time, so it never runs a job the app is running.

use std::process::ExitCode;
use std::time::Duration;

use databrain_workspace::{JobRun, Workspace};

use crate::background;
use crate::jobs;

/// Longest one `tick` runs: then the runs still going are stopped and it
/// exits. The OS doesn't start a new `tick` while one runs, so a run stuck
/// on a dead connection (with no time limit) would otherwise stop every
/// background schedule for good.
pub const TICK_DEADLINE: Duration = Duration::from_secs(55 * 60);

/// Longest `stop` waits for the run to end.
const STOP_WAIT: Duration = Duration::from_secs(60);

/// Why the runs still going at [`TICK_DEADLINE`] were stopped.
const DEADLINE_STOP: &str =
    "Stopped: the background runner ran for its longest time (55 minutes) and stopped its runs; set the job's time limits, or run it in DataBrain";

/// One background pass: start the due jobs, wait for them and the jobs that
/// run after them, then look again so missed runs made up one by one
/// ("each") don't wait a minute apart; at most `deadline` in all (then the
/// runs still going are stopped). Records the time of each pass for
/// Settings. Returns the runs this process started, as saved when they
/// ended, oldest first: never runs of another process (the app).
pub async fn background_tick(state: &crate::api::AppState, deadline: Duration) -> Vec<JobRun> {
    let ctx = state.job_ctx();
    let _ = ctx.jobs.take_started();
    let until = tokio::time::Instant::now() + deadline;
    let pass = || {
        let started = jobs::tick_as(&ctx, "background");
        let _ = state.workspace.set_setting(background::LAST_TICK_SETTING, &serde_json::json!(databrain_workspace::now_ms()));
        started
    };
    // Runs (and the jobs that run after them) all ended before the deadline.
    let wait = || async { tokio::time::timeout_at(until, jobs::wait_all(&ctx)).await.is_ok() };
    let mut started = pass();
    let mut in_time = true;
    for _ in 0..=databrain_workspace::jobs::MISSED_KEPT {
        if started.is_empty() && ctx.jobs.running_ids().is_empty() {
            break;
        }
        in_time = wait().await;
        if !in_time {
            break;
        }
        started = pass();
    }
    // Runs the last pass started: the runner never exits under them (a load
    // would stop half-way, the run stay "running")…
    if in_time {
        in_time = wait().await;
    }
    // …unless the deadline came: they are stopped, and end within the stop grace.
    if !in_time {
        for id in ctx.jobs.running_ids() {
            tracing::warn!(job = %id, "background runner deadline reached: stopping the run");
            ctx.jobs.stop_for(&id, DEADLINE_STOP);
        }
        let _ = tokio::time::timeout(Duration::from_secs(120), jobs::wait_all(&ctx)).await;
    }
    ctx.jobs.take_started().into_iter().filter_map(|(job, run)| state.workspace.job_run(&job, run).ok().flatten()).collect()
}

/// What `tick` prints, the desktop notifications it shows, and whether it
/// fails (exit code 1).
#[derive(Debug, Default, PartialEq)]
pub struct TickReport {
    pub lines: Vec<String>,
    /// (title, text) per run that failed or was stopped.
    pub notices: Vec<(String, String)>,
    /// A run failed.
    pub failed: bool,
}

/// [`TickReport`] of the runs a background pass started: each run (a job
/// made up twice reports both), a notification for each that failed or was
/// stopped (a lost lease, the deadline).
pub fn tick_report(ws: &Workspace, runs: &[JobRun]) -> TickReport {
    let mut r = TickReport::default();
    for run in runs {
        let name = ws.get_job(&run.job_id).map(|j| j.name).unwrap_or_else(|_| run.job_id.clone());
        let after = run.trigger.strip_prefix(jobs::AFTER_TRIGGER);
        let what = match after {
            Some(up) => format!("{name} (after \"{up}\")"),
            None => name.clone(),
        };
        r.lines.push(format!("{what}: {}", run.status));
        let why = run.error.clone().unwrap_or_else(|| "see its runs in DataBrain".into());
        match run.status.as_str() {
            "error" => {
                r.failed = true;
                let title = match after {
                    Some(up) => format!("DataBrain: job run after \"{up}\" failed"),
                    None => "DataBrain: scheduled job failed".into(),
                };
                r.notices.push((title, format!("{name}: {why}")));
            }
            "cancelled" => r.notices.push(("DataBrain: scheduled job stopped".into(), format!("{name}: {why}"))),
            _ => {}
        }
    }
    r
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: databrain-jobs tick [--force] | run <job> [--set <parameter set>] [--param name=value]... | list | runs <job> [--limit N] | stop <job> [--no-wait]\n       (also dbr jobs …; <job> is its name, any case, or id)"
    );
    ExitCode::from(2)
}

/// The job `which` names (id, or name in any case), else why not.
fn find_job(state: &crate::api::AppState, which: &str) -> Result<databrain_workspace::JobSummary, String> {
    let list = state.workspace.list_jobs().unwrap_or_default();
    list.iter()
        .find(|j| j.id == which)
        .or_else(|| list.iter().find(|j| j.name.eq_ignore_ascii_case(which)))
        .cloned()
        .ok_or_else(|| format!("no job named {which:?} (databrain-jobs list shows them)"))
}

/// "1m 5s", "12s", "850 ms".
fn took(ms: i64) -> String {
    if ms < 1000 {
        format!("{} ms", ms.max(0))
    } else if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// A job's last runs, one a line, then the latest's steps: what `runs` prints.
pub fn runs_text(runs: &[JobRun], now: i64) -> String {
    let mut out = String::new();
    if runs.is_empty() {
        return "never ran\n".into();
    }
    for r in runs {
        let ms = r.finished_at.unwrap_or(now) - r.started_at;
        let tail = match (&r.error, r.status.as_str()) {
            (Some(e), _) => format!("  {}", e.lines().next().unwrap_or_default()),
            (None, "running") => "  (running)".into(),
            _ => String::new(),
        };
        out.push_str(&format!("#{:<6} {}  {:<10} {:<9} {:>8}{tail}\n", r.id, when(r.started_at), r.trigger, r.status, took(ms)));
    }
    let latest = &runs[0];
    out.push_str(&format!("steps of #{}:\n", latest.id));
    for n in &latest.nodes {
        let what = n.summary.error.clone().or_else(|| n.summary.rows.map(|r| format!("{r} rows"))).unwrap_or_default();
        out.push_str(&format!("  {:<24} {:<10} {}\n", n.name, n.summary.status, what.lines().next().unwrap_or_default()));
    }
    out
}

fn when(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

/// Run a jobs command (`tick`, `run <job>`, `list`); `args` without the program name.
pub fn main(args: Vec<String>) -> ExitCode {
    // The program has no console window on Windows (the task runs `tick`
    // every minute); `run` and `list` attach to the terminal they were
    // started from. SAFETY: AttachConsole takes a process id constant and
    // touches no Rust memory; failing (no parent console) is fine.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    match super::runtime() {
        Ok(rt) => rt.block_on(run(args)),
        Err(e) => {
            eprintln!("databrain-jobs: {e}");
            ExitCode::from(2)
        }
    }
}

async fn run(args: Vec<String>) -> ExitCode {
    let Some(cmd) = args.first().map(String::as_str) else { return usage() };
    let dir = background::data_dir();
    crate::logging::init("databrain-jobs", Some(&dir), false);
    let state = match background::headless_state(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("databrain-jobs: {}", e.message);
            return ExitCode::from(2);
        }
    };
    let code = command(cmd, &args, &state).await;
    // Results and DuckDB work kept on disk don't outlive the run.
    crate::api::remove_spill_folders(&state.engine);
    code
}

async fn command(cmd: &str, args: &[String], state: &crate::api::AppState) -> ExitCode {
    let ctx = state.job_ctx();
    match cmd {
        "tick" => {
            // Off in the app's settings: nothing to do (an OS entry left behind).
            if !background::enabled(&state.workspace) && !args.iter().any(|a| a == "--force") {
                return ExitCode::SUCCESS;
            }
            let runs = background_tick(state, TICK_DEADLINE).await;
            let report = tick_report(&state.workspace, &runs);
            for line in &report.lines {
                println!("{} {line}", when(databrain_workspace::now_ms()));
            }
            for (title, body) in &report.notices {
                background::notify(title, body);
            }
            if report.failed { ExitCode::from(1) } else { ExitCode::SUCCESS }
        }
        "run" => {
            let Some(which) = args.get(1) else { return usage() };
            let list = state.workspace.list_jobs().unwrap_or_default();
            let Some(job) = list.iter().find(|j| &j.id == which).or_else(|| list.iter().find(|j| j.name.eq_ignore_ascii_case(which))) else {
                eprintln!("databrain-jobs: no job named {which:?} (databrain-jobs list shows them)");
                return ExitCode::from(2);
            };
            // `--set NAME` and `--param name=value` (repeatable) for this run.
            let mut over = jobs::RunParams::default();
            let mut rest = args[2..].iter();
            while let Some(a) = rest.next() {
                match a.as_str() {
                    "--set" => over.set = rest.next().cloned(),
                    "--param" => match rest.next().and_then(|p| p.split_once('=')) {
                        Some((k, v)) => {
                            over.values.insert(
                                k.trim().trim_start_matches(':').to_string(),
                                databrain_connector_core::sql::ParamValue { value: v.to_string(), raw: false },
                            );
                        }
                        None => {
                            eprintln!("databrain-jobs: --param takes name=value");
                            return ExitCode::from(2);
                        }
                    },
                    other => {
                        eprintln!("databrain-jobs: unknown option {other:?}");
                        return usage();
                    }
                }
            }
            let over = (over.set.is_some() || !over.values.is_empty()).then_some(over);
            println!("Running {}…", job.name);
            match jobs::run_and_wait_with(&ctx, &job.id, "cli", over).await {
                Ok(run) => {
                    for n in &run.nodes {
                        println!(
                            "  {:<24} {:<10} {}",
                            n.name,
                            n.summary.status,
                            n.summary.rows.map(|r| format!("{r} rows")).or(n.summary.error.clone()).unwrap_or_default()
                        );
                    }
                    println!("{}: {}", job.name, run.status);
                    // Jobs that run after this one (it succeeded) run before the command ends.
                    let mut ok = run.status == "success";
                    for id in jobs::wait_all(&ctx).await {
                        let Some(after) = state.workspace.job_runs(&id, 1).ok().and_then(|r| r.into_iter().next()) else { continue };
                        let name = state.workspace.get_job(&id).map(|j| j.name).unwrap_or(id);
                        println!("{name} (after {}): {}", job.name, after.status);
                        ok &= after.status == "success";
                    }
                    if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
                }
                Err(e) => {
                    eprintln!("databrain-jobs: {}", e.message);
                    ExitCode::from(2)
                }
            }
        }
        "runs" => {
            let Some(which) = args.get(1) else { return usage() };
            let job = match find_job(state, which) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("databrain-jobs: {e}");
                    return ExitCode::from(2);
                }
            };
            let limit = match args.iter().position(|a| a == "--limit").map(|i| args.get(i + 1).and_then(|n| n.parse::<u32>().ok())) {
                None => 5,
                Some(Some(n)) if n > 0 => i64::from(n),
                Some(_) => {
                    eprintln!("databrain-jobs: --limit takes a number of runs");
                    return ExitCode::from(2);
                }
            };
            let runs = state.workspace.job_runs(&job.id, limit).unwrap_or_default();
            print!("{}", runs_text(&runs, databrain_workspace::now_ms()));
            ExitCode::SUCCESS
        }
        "stop" => {
            let Some(which) = args.get(1) else { return usage() };
            let job = match find_job(state, which) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("databrain-jobs: {e}");
                    return ExitCode::from(2);
                }
            };
            let run = match jobs::request_stop(&ctx, &job.id, "terminal") {
                Ok(jobs::StopAsked::NotRunning) => {
                    eprintln!("databrain-jobs: {} isn't running", job.name);
                    return ExitCode::from(1);
                }
                Ok(jobs::StopAsked::Stopped) => return ExitCode::SUCCESS,
                Ok(jobs::StopAsked::Asked(run)) => run,
                Err(e) => {
                    eprintln!("databrain-jobs: {}", e.message);
                    return ExitCode::from(2);
                }
            };
            if args.iter().any(|a| a == "--no-wait") {
                println!("Asked {} (run #{run}) to stop", job.name);
                return ExitCode::SUCCESS;
            }
            println!("Stopping {} (run #{run})…", job.name);
            // The runner reads the request within a few seconds; a statement
            // may take longer to stop on its server.
            let until = tokio::time::Instant::now() + STOP_WAIT;
            loop {
                let r = state.workspace.job_run(&job.id, run).ok().flatten();
                match r {
                    Some(r) if r.status != "running" => {
                        println!("{}: {}", job.name, r.error.as_deref().unwrap_or(&r.status));
                        return ExitCode::SUCCESS;
                    }
                    _ if tokio::time::Instant::now() >= until => {
                        eprintln!("databrain-jobs: {} was asked to stop and hasn't ended yet (a statement can take a while to stop on its server)", job.name);
                        return ExitCode::from(1);
                    }
                    _ => tokio::time::sleep(Duration::from_millis(500)).await,
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

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_workspace::{NodeRunRecord, NodeRunSummary};

    #[test]
    fn runs_one_a_line_then_the_latests_steps() {
        let step = |name: &str, status: &str, rows: Option<i64>, error: Option<&str>| NodeRunRecord {
            node_id: name.into(),
            name: name.into(),
            summary: NodeRunSummary { status: status.into(), rows, error: error.map(str::to_string), ..Default::default() },
        };
        let run = |id: i64, status: &str, ms: Option<i64>, error: Option<&str>, nodes: Vec<NodeRunRecord>| JobRun {
            id,
            job_id: "j".into(),
            trigger: "manual".into(),
            started_at: 0,
            finished_at: ms,
            status: status.into(),
            nodes,
            error: error.map(str::to_string),
            params: Default::default(),
            ai: false,
        };
        let text = runs_text(
            &[
                run(
                    42,
                    "error",
                    Some(65_000),
                    Some("load: permission denied\nmore"),
                    vec![step("read", "success", Some(1200), None), step("load", "error", None, Some("permission denied"))],
                ),
                run(41, "success", Some(850), None, vec![]),
            ],
            0,
        );
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].starts_with("#42") && lines[0].contains("error") && lines[0].contains("1m 5s") && lines[0].ends_with("load: permission denied"),
            "{text}"
        );
        assert!(lines[1].starts_with("#41") && lines[1].contains("850 ms"), "{text}");
        assert_eq!(lines[2], "steps of #42:");
        assert!(lines[3].contains("read") && lines[3].contains("1200 rows"), "{text}");
        assert!(lines[4].contains("load") && lines[4].contains("permission denied"), "{text}");
        assert_eq!(runs_text(&[], 0), "never ran\n");
    }
}
