//! `dbr`: starts `databrain query …` (see `databrain_app::cli::query`): SQL
//! from a terminal on a saved connection, the rows printed there.

// Windows: a console program (it prints to the terminal it runs in).
#[path = "../launcher.rs"]
mod launcher;

fn main() -> std::process::ExitCode {
    launcher::launch("query", "dbr")
}
