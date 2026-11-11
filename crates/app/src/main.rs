#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
    // `databrain mcp` / `databrain jobs …` (or started through the
    // databrain-mcp / databrain-jobs launchers); else the app.
    if let Some(code) = databrain_app::cli::dispatch(std::env::args_os().collect()) {
        return code;
    }
    databrain_app::run();
    ExitCode::SUCCESS
}
