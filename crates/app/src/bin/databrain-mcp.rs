//! `databrain-mcp`: MCP stdio server exposing DataBrain connections that have
//! "Allow external agents (MCP)" enabled. Configure it in Kiro CLI / Claude
//! Code as a stdio server; it reads the same workspace and keychain as the app.

use std::sync::Arc;

use databrain_ai::mcp::McpServer;
use databrain_auth::SwitchableStore;
use databrain_query_engine::{EventHub, EventSink, JobEvent, QueryEngine};
use databrain_result_store::ResultStore;
use databrain_workspace::Workspace;

struct Quiet;
impl EventSink for Quiet {
    fn emit(&self, _: JobEvent) {}
}

fn workspace_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("DATABRAIN_WORKSPACE") {
        return p.into();
    }
    dirs::data_dir().unwrap_or_else(|| ".".into()).join("dev.databrain.app").join("workspace.db")
}

#[tokio::main]
async fn main() {
    let path = workspace_path();
    let ws = match Workspace::open(&path) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            eprintln!("databrain-mcp: cannot open workspace {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let hub = EventHub::new(Arc::new(Quiet));
    let slot = databrain_connector_core::external::ExternalTablesSlot::new();
    // Same credential store as the app (keychain or the local vault).
    let vault_dir = path.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    let secrets = Arc::new(SwitchableStore::new(databrain_app::api::saved_store_kind(&ws), &vault_dir));
    let engine = QueryEngine::new(databrain_app::api::registry_with_outputs(Some(slot.clone())), ws, secrets, Arc::new(ResultStore::new()), hub.clone());
    slot.set(engine.external_tables());
    // Pinned outputs saved by the app are readable (read-only use) here too.
    if let Some(dir) = path.parent() {
        engine.outputs().set_snapshot_dir(dir.join("outputs"));
    }
    let server = McpServer { engine, hub };
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    if let Err(e) = server.serve(stdin, tokio::io::stdout()).await {
        eprintln!("databrain-mcp: {e}");
    }
}
