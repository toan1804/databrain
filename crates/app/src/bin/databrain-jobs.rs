//! `databrain-jobs`: starts `databrain jobs …` (see `databrain_app::cli::jobs`):
//! `tick`, `run <job>`, `list`. The OS scheduler starts `databrain-jobs tick`
//! every minute when "Run scheduled jobs when DataBrain is closed" is on.

// No console window every minute on Windows (the task runs `tick`); `run`
// and `list` attach to the terminal they were started from, and so does the
// `databrain` they start (it attaches to this launcher's console).
#![cfg_attr(windows, windows_subsystem = "windows")]

#[path = "../launcher.rs"]
mod launcher;

fn main() -> std::process::ExitCode {
    // SAFETY: AttachConsole takes a process id constant and touches no Rust
    // memory; failing (no parent console) is fine.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    launcher::launch("jobs", "databrain-jobs")
}
