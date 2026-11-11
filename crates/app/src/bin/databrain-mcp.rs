//! `databrain-mcp`: starts `databrain mcp`, DataBrain's MCP stdio server
//! (see `databrain_app::cli::mcp`). MCP clients are set up with this path.

#[path = "../launcher.rs"]
mod launcher;

fn main() -> std::process::ExitCode {
    launcher::launch("mcp", "databrain-mcp")
}
