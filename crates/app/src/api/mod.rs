//! Backend API used by the Tauri commands. Plain async functions over
//! [`AppState`] so the whole flow can be tested without a webview.
//!
//! Text offsets exchanged with the UI are UTF-16 code units (JavaScript string
//! indices); Rust-side SQL spans are byte offsets. Conversion happens here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use databrain_auth::{SecretRef, SecretStore};
use databrain_connector_core::sql::{split_statements, statement_at};
use databrain_connector_core::{ConnectionConfig, ConnectorInfo, ConnectorKind, ConnectorRegistry, DbObject, ObjectDetail, SchemaInfo};
use databrain_export::{ExportOptions, export_file, export_to_string};
use databrain_query_engine::{EngineError, EventHub, EventSink, QueryEngine, Result, RunRequest, RunResponse, TestResult};
use databrain_result_store::{ColumnStats, FindResult, Page, ResultInfo, ResultStore, ViewSpec};
use databrain_workspace::{ConnectionProfile, Folder, FolderKind, HistoryEntry, HistoryQuery, SavedQuery, TabState, Workspace};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

mod connections;
mod drivers;
mod explorer;
mod jobs;
mod library;
mod lineage;
mod outputs;
mod queries;
mod results;
pub use connections::*;
pub use drivers::*;
pub use explorer::*;
pub use jobs::*;
pub use library::*;
pub use lineage::*;
pub use outputs::*;
pub use queries::*;
pub use results::*;

/// Largest page the grid may request at once.
const MAX_PAGE: usize = 5_000;
/// Largest number of rows copied to the clipboard at once.
const MAX_COPY_ROWS: usize = 100_000;

pub struct AppState {
    pub engine: Arc<QueryEngine>,
    pub workspace: Arc<Workspace>,
    pub secrets: Arc<dyn SecretStore>,
    pub hub: Arc<EventHub>,
    pub ui: Arc<dyn crate::ai_api::UiBridge>,
    pub ai: crate::ai_api::AiState,
    pub jobs: Arc<crate::jobs::JobsState>,
    /// Explorer cache writes, after the explorer has its answer.
    pub meta: Arc<crate::meta_writer::MetaWriter>,
    /// Set when `secrets` is the switchable keychain/vault store.
    pub credential_store: parking_lot::Mutex<Option<(Arc<databrain_auth::SwitchableStore>, PathBuf)>>,
}

/// Where DataBrain keeps passwords and tokens.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct CredentialStoreView {
    pub kind: databrain_auth::StoreKind,
    /// Folder of `vault.json` / `vault.key`.
    pub vault_dir: Option<String>,
    pub switchable: bool,
}

/// Setting key for the credential store choice.
pub const CREDENTIAL_STORE_SETTING: &str = "credential_store";

/// Read the saved store choice (keychain when unset).
pub fn saved_store_kind(ws: &Workspace) -> databrain_auth::StoreKind {
    ws.get_setting(CREDENTIAL_STORE_SETTING).ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

/// Every secret reference the workspace can own (for migration).
fn all_secret_refs(state: &AppState) -> Result<Vec<SecretRef>> {
    let mut refs = Vec::new();
    for c in state.workspace.list_connections()? {
        refs.push(SecretRef::for_connection(&c.id));
        for slot in SECRET_SLOTS.iter().chain(["oauth"].iter()) {
            refs.push(SecretRef::slot(&c.id, slot));
        }
    }
    for p in state.workspace.list_ai_providers()? {
        refs.push(SecretRef::for_ai_provider(&p.id));
        refs.push(SecretRef::slot(&format!("ai:{}", p.id), "oauth"));
    }
    Ok(refs)
}

pub fn credential_store(state: &AppState) -> CredentialStoreView {
    match &*state.credential_store.lock() {
        Some((s, dir)) => CredentialStoreView { kind: s.kind(), vault_dir: Some(dir.to_string_lossy().into_owned()), switchable: true },
        None => CredentialStoreView { kind: databrain_auth::StoreKind::Keychain, vault_dir: None, switchable: false },
    }
}

/// Switch between the OS keychain and the local vault, moving saved secrets.
/// Reading from the keychain can show one macOS prompt per saved secret
/// (once); afterwards the vault needs no prompts.
pub async fn set_credential_store(state: &AppState, kind: databrain_auth::StoreKind) -> Result<databrain_auth::MigrationReport> {
    let store =
        state.credential_store.lock().as_ref().map(|(s, _)| s.clone()).ok_or_else(|| invalid("the credential store cannot be changed in this build"))?;
    let refs = all_secret_refs(state)?;
    let report = blocking(move || store.switch(kind, &refs).map_err(EngineError::from)).await?;
    state.workspace.set_setting(CREDENTIAL_STORE_SETTING, &serde_json::to_value(kind).unwrap_or_default())?;
    Ok(report)
}

pub fn default_registry() -> ConnectorRegistry {
    registry_with_outputs(None)
}

/// Registry whose DuckDB connector can query outputs (`results.<name>`).
pub fn registry_with_outputs(#[allow(unused_variables)] outputs: Option<Arc<databrain_connector_core::external::ExternalTablesSlot>>) -> ConnectorRegistry {
    #[allow(unused_mut)]
    let mut r = ConnectorRegistry::new();
    #[cfg(feature = "sqlite")]
    r.register(Arc::new(databrain_connector_sqlite::SqliteConnector::new()));
    #[cfg(feature = "postgres")]
    r.register(Arc::new(databrain_connector_postgres::PostgresConnector::new()));
    #[cfg(feature = "mysql")]
    r.register(Arc::new(databrain_connector_mysql::MysqlConnector::new()));
    #[cfg(feature = "mssql")]
    r.register(Arc::new(databrain_connector_mssql::MssqlConnector::new()));
    #[cfg(feature = "oracle")]
    r.register(Arc::new(databrain_connector_oracle::OracleConnector::new()));
    #[cfg(feature = "snowflake")]
    r.register(Arc::new(databrain_connector_cloud::SnowflakeConnector::new()));
    #[cfg(feature = "databricks")]
    r.register(Arc::new(databrain_connector_cloud::DatabricksConnector::new()));
    #[cfg(feature = "bigquery")]
    r.register(Arc::new(databrain_connector_cloud::BigQueryConnector::new()));
    #[cfg(feature = "mongodb")]
    r.register(Arc::new(databrain_connector_mongodb::MongoConnector));
    // Classification and table rules read MongoDB commands with the parser that runs them.
    #[cfg(feature = "mongodb")]
    databrain_connector_core::language::set_mongo_analyzer(databrain_connector_mongodb::analyze);
    #[cfg(feature = "neo4j")]
    r.register(Arc::new(databrain_connector_neo4j::Neo4jConnector));
    #[cfg(feature = "clickhouse")]
    r.register(Arc::new(databrain_connector_clickhouse::ClickhouseConnector::new()));
    #[cfg(feature = "duckdb-engine")]
    r.register(Arc::new(match outputs {
        Some(slot) => databrain_connector_duckdb::DuckdbConnector::with_outputs(slot),
        None => databrain_connector_duckdb::DuckdbConnector::new(),
    }));
    r
}

impl AppState {
    pub fn new(workspace: Arc<Workspace>, secrets: Arc<dyn SecretStore>, events: Arc<dyn EventSink>, ui: Arc<dyn crate::ai_api::UiBridge>) -> Self {
        let hub = EventHub::new(events);
        let slot = databrain_connector_core::external::ExternalTablesSlot::new();
        let engine = QueryEngine::new(registry_with_outputs(Some(slot.clone())), workspace.clone(), secrets.clone(), Arc::new(ResultStore::new()), hub.clone());
        slot.set(engine.external_tables());
        // Tabs left in manual-commit mode.
        for t in workspace.list_tabs().unwrap_or_default() {
            if t.auto_commit == Some(false) {
                engine.set_manual_commit(&t.id, true);
            }
        }
        let ai = crate::ai_api::AiState::default();
        engine.set_interaction(Arc::new(crate::ai_api::AppInteraction::new(ui.clone(), ai.sign_in_cancel.clone())));
        Self {
            engine,
            workspace,
            secrets,
            hub,
            ui,
            ai,
            jobs: Arc::new(crate::jobs::JobsState::default()),
            meta: Arc::default(),
            credential_store: parking_lot::Mutex::new(None),
        }
    }

    /// Use the switchable keychain/vault store (`secrets` must be the same store).
    pub fn set_credential_store(&self, store: Arc<databrain_auth::SwitchableStore>, vault_dir: PathBuf) {
        *self.credential_store.lock() = Some((store, vault_dir));
    }

    /// Move passwords saved in MongoDB/Neo4j connection strings (before
    /// they went to the keychain on save) out of workspace.db. Returns how
    /// many connections changed.
    pub fn move_uri_passwords(&self) -> usize {
        let mut n = 0;
        for p in self.workspace.list_connections().unwrap_or_default() {
            if crate::conn_io::take_uri_password(&mut p.config.clone()).is_none() {
                continue;
            }
            let args = SaveConnectionArgs { profile: p, secret: None, clear_secret: false, extra_secrets: Default::default() };
            if store_connection(self, args).is_ok() {
                n += 1;
            }
        }
        n
    }

    fn results(&self) -> &Arc<ResultStore> {
        self.engine.results()
    }

    /// Save pinned outputs under `dir` and restore the ones saved earlier.
    pub fn set_snapshot_dir(&self, dir: PathBuf) {
        self.engine.outputs().set_snapshot_dir(dir);
    }
}

fn invalid(msg: impl Into<String>) -> EngineError {
    EngineError::new("invalid", msg)
}

/// Run CPU-heavy work off the async runtime threads.
async fn blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| EngineError::new("internal", e.to_string()))?
}

// ------------------------------------------------------------ offsets

pub fn byte_to_utf16(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    let mut b = byte;
    while !text.is_char_boundary(b) {
        b -= 1;
    }
    text[..b].encode_utf16().count()
}

pub fn utf16_to_byte(text: &str, units: usize) -> usize {
    let mut count = 0;
    for (i, ch) in text.char_indices() {
        if count >= units {
            return i;
        }
        count += ch.len_utf16();
    }
    text.len()
}

#[cfg(test)]
mod tests;
