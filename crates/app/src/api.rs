//! Backend API used by the Tauri commands. Plain async functions over
//! [`AppState`] so the whole flow can be tested without a webview.
//!
//! Text offsets exchanged with the UI are UTF-16 code units (JavaScript string
//! indices); Rust-side SQL spans are byte offsets. Conversion happens here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use databrain_auth::{SecretRef, SecretStore};
use databrain_connector_core::sql::{split_statements, statement_at};
use databrain_connector_core::{
    ConnectionConfig, ConnectorInfo, ConnectorKind, ConnectorRegistry, DbObject, ObjectDetail,
    SchemaInfo,
};
use databrain_export::{ExportOptions, export_file, export_to_string};
use databrain_query_engine::{
    EngineError, EventHub, EventSink, QueryEngine, Result, RunRequest, RunResponse, TestResult,
};
use databrain_result_store::{ColumnStats, FindResult, Page, ResultInfo, ResultStore, ViewSpec};
use databrain_workspace::{
    ConnectionProfile, Folder, FolderKind, HistoryEntry, HistoryQuery, SavedQuery, TabState,
    Workspace,
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

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
    /// Set when `secrets` is the switchable keychain/vault store.
    pub credential_store: parking_lot::Mutex<Option<(Arc<databrain_auth::SwitchableStore>, PathBuf)>>,
}

/// Where DataBrain keeps passwords and tokens.
#[derive(Debug, Clone, Serialize)]
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
    let store = state.credential_store.lock().as_ref().map(|(s, _)| s.clone()).ok_or_else(|| invalid("the credential store cannot be changed in this build"))?;
    let refs = all_secret_refs(state)?;
    let report = blocking(move || store.switch(kind, &refs).map_err(EngineError::from)).await?;
    state.workspace.set_setting(CREDENTIAL_STORE_SETTING, &serde_json::to_value(kind).unwrap_or_default())?;
    Ok(report)
}

pub fn default_registry() -> ConnectorRegistry {
    registry_with_outputs(None)
}

/// Registry whose DuckDB connector can query outputs (`results.<name>`).
pub fn registry_with_outputs(
    #[allow(unused_variables)] outputs: Option<Arc<databrain_connector_core::external::ExternalTablesSlot>>,
) -> ConnectorRegistry {
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
    #[cfg(feature = "duckdb")]
    r.register(Arc::new(match outputs {
        Some(slot) => databrain_connector_duckdb::DuckdbConnector::with_outputs(slot),
        None => databrain_connector_duckdb::DuckdbConnector::new(),
    }));
    r
}

impl AppState {
    pub fn new(
        workspace: Arc<Workspace>,
        secrets: Arc<dyn SecretStore>,
        events: Arc<dyn EventSink>,
        ui: Arc<dyn crate::ai_api::UiBridge>,
    ) -> Self {
        let hub = EventHub::new(events);
        let slot = databrain_connector_core::external::ExternalTablesSlot::new();
        let engine = QueryEngine::new(
            registry_with_outputs(Some(slot.clone())),
            workspace.clone(),
            secrets.clone(),
            Arc::new(ResultStore::new()),
            hub.clone(),
        );
        slot.set(engine.external_tables());
        let ai = crate::ai_api::AiState::default();
        engine.set_interaction(Arc::new(crate::ai_api::AppInteraction::new(ui.clone(), ai.sign_in_cancel.clone())));
        Self {
            engine,
            workspace,
            secrets,
            hub,
            ui,
            ai,
            credential_store: parking_lot::Mutex::new(None),
        }
    }

    /// Use the switchable keychain/vault store (`secrets` must be the same store).
    pub fn set_credential_store(&self, store: Arc<databrain_auth::SwitchableStore>, vault_dir: PathBuf) {
        *self.credential_store.lock() = Some((store, vault_dir));
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
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| EngineError::new("internal", e.to_string()))?
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

// ------------------------------------------------------------ connectors & connections

pub fn list_connectors(state: &AppState) -> Vec<ConnectorInfo> {
    state.engine.registry().infos()
}

#[derive(Debug, Serialize)]
pub struct ConnectionView {
    #[serde(flatten)]
    pub profile: ConnectionProfile,
    pub connected: bool,
}

pub fn list_connections(state: &AppState) -> Result<Vec<ConnectionView>> {
    let connected = state.engine.connected_ids();
    Ok(state
        .workspace
        .list_connections()?
        .into_iter()
        .map(|p| ConnectionView {
            connected: connected.contains(&p.id),
            profile: p,
        })
        .collect())
}

/// Secondary secret slots a connection may use.
pub const SECRET_SLOTS: &[&str] = &["ssh", "client_secret", "passphrase"];

#[derive(Debug, Deserialize)]
pub struct SaveConnectionArgs {
    pub profile: ConnectionProfile,
    /// New password/token/key. `None` keeps the current secret.
    #[serde(default)]
    pub secret: Option<String>,
    /// Remove the stored secret.
    #[serde(default)]
    pub clear_secret: bool,
    /// Secondary secrets by slot (`ssh`, `client_secret`, `passphrase`).
    /// Empty string removes the slot; absent keeps it.
    #[serde(default)]
    pub extra_secrets: std::collections::BTreeMap<String, String>,
}

pub fn save_connection(state: &AppState, args: SaveConnectionArgs) -> Result<ConnectionProfile> {
    let mut profile = args.profile;
    validate_config(&profile.config)?;
    if args.secret.is_some() {
        profile.has_secret = true;
    } else if args.clear_secret {
        profile.has_secret = false;
    } else if !profile.id.is_empty() {
        // Keep the flag from the stored record; the UI may not know it.
        if let Ok(existing) = state.workspace.get_connection(&profile.id) {
            profile.has_secret = existing.has_secret;
        }
    }
    let saved = state.workspace.save_connection(profile)?;
    let r = SecretRef::for_connection(&saved.id);
    if let Some(secret) = args.secret {
        if let Err(e) = state.secrets.set(&r, &SecretString::from(secret)) {
            let mut revert = saved.clone();
            revert.has_secret = false;
            let _ = state.workspace.save_connection(revert);
            return Err(e.into());
        }
    } else if args.clear_secret {
        state.secrets.delete(&r)?;
    }
    for (slot, value) in &args.extra_secrets {
        if !SECRET_SLOTS.contains(&slot.as_str()) {
            continue;
        }
        let sr = SecretRef::slot(&saved.id, slot);
        if value.is_empty() {
            state.secrets.delete(&sr)?;
        } else {
            state.secrets.set(&sr, &SecretString::from(value.clone()))?;
        }
    }
    // Apply new settings (and auth method) on the next query.
    state.engine.disconnect(&saved.id);
    state.engine.forget_credentials(&saved.id);
    Ok(saved)
}

fn validate_config(cfg: &ConnectionConfig) -> Result<()> {
    use databrain_auth::AuthMethod;
    match cfg.kind {
        ConnectorKind::Duckdb => {}
        ConnectorKind::Sqlite => {
            if cfg.file_path.as_deref().is_none_or(|p| p.trim().is_empty()) {
                return Err(invalid("Choose a database file"));
            }
        }
        ConnectorKind::Snowflake if cfg.opt("account").is_none() && cfg.host.as_deref().is_none_or(str::is_empty) => {
            return Err(invalid("Snowflake account identifier is required"));
        }
        ConnectorKind::Databricks if cfg.opt("http_path").is_none() && cfg.opt("warehouse_id").is_none() => {
            return Err(invalid("SQL warehouse HTTP path is required"));
        }
        ConnectorKind::Bigquery if cfg.opt("project").is_none() => {
            return Err(invalid("Billing project ID is required"));
        }
        _ => {}
    }
    match &cfg.auth {
        AuthMethod::Password { user } | AuthMethod::KeyPair { user } | AuthMethod::ExternalBrowser { user } if user.trim().is_empty() => {
            return Err(invalid("User name is required"));
        }
        _ => {}
    }
    if let Some(ssh) = &cfg.ssh {
        if cfg.kind.is_cloud() || matches!(cfg.kind, ConnectorKind::Sqlite | ConnectorKind::Duckdb) {
            return Err(invalid("SSH tunnels are only available for server databases"));
        }
        if ssh.host.trim().is_empty() || ssh.user.trim().is_empty() {
            return Err(invalid("SSH host and user are required"));
        }
    }
    Ok(())
}

pub fn move_to_folder(state: &AppState, kind: FolderKind, item_id: &str, folder_id: Option<String>) -> Result<()> {
    Ok(state.workspace.move_to_folder(kind, item_id, folder_id.as_deref())?)
}

/// Delete a connection: closes its sessions, removes it, its AI knowledge
/// and every secret it owns (password, SSH, client secret, key passphrase,
/// cached OAuth tokens). Saved queries, history and notebooks are kept.
pub fn delete_connection(state: &AppState, id: &str) -> Result<()> {
    state.engine.disconnect(id);
    state.engine.forget_credentials(id);
    state.workspace.delete_connection(id)?;
    // The connection is gone either way; a secret that cannot be removed
    // now is unreachable, so report but don't fail.
    let mut errors = Vec::new();
    let refs = std::iter::once(SecretRef::for_connection(id)).chain(SECRET_SLOTS.iter().chain(["oauth"].iter()).map(|s| SecretRef::slot(id, s)));
    for r in refs {
        if let Err(e) = state.secrets.delete(&r) {
            errors.push(e.to_string());
        }
    }
    if let Some(e) = errors.first() {
        return Err(EngineError::new("partial", format!("connection deleted, but a saved secret could not be removed: {e}")));
    }
    Ok(())
}

pub async fn test_connection(
    state: &AppState,
    config: ConnectionConfig,
    secret: Option<String>,
    connection_id: Option<String>,
    ssh_secret: Option<String>,
) -> Result<TestResult> {
    validate_config(&config)?;
    state
        .engine
        .test_connection(&config, secret, connection_id.as_deref(), ssh_secret)
        .await
}

pub async fn connect(state: &AppState, id: &str) -> Result<String> {
    state.engine.connect(id).await
}

pub fn disconnect(state: &AppState, id: &str) {
    state.engine.disconnect(id);
}

// ------------------------------------------------------------ explorer

pub async fn list_schemas(state: &AppState, id: &str) -> Result<Vec<SchemaInfo>> {
    state.engine.list_schemas(id).await
}

pub async fn list_objects(state: &AppState, id: &str, schema: &str) -> Result<Vec<DbObject>> {
    let objs = state.engine.list_objects(id, schema).await?;
    // Remember it for completion / search (best effort).
    let _ = state.engine.workspace().meta_put_schema(id, schema, &objs);
    Ok(objs)
}

/// Catalog search: tables and views whose name contains `query`
/// (`schema.part` narrows by schema).
pub async fn search_objects(state: &AppState, id: &str, query: &str, limit: Option<usize>) -> Result<Vec<DbObject>> {
    if query.trim().is_empty() {
        return Ok(vec![]);
    }
    let hits = state.engine.search_objects(id, query, limit.unwrap_or(100)).await?;
    let _ = state.engine.workspace().meta_add_objects(id, &hits);
    Ok(hits)
}

/// Completion, local part: tables from the knowledge index (no network, instant).
pub fn complete_tables_local(state: &AppState, id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let rows = state.engine.workspace().kn_complete(id, schema, query, limit.clamp(1, 500))?;
    Ok(rows
        .into_iter()
        .filter_map(|(schema, name, kind, comment)| {
            let kind: databrain_connector_core::ObjectKind = serde_json::from_value(serde_json::Value::String(kind)).unwrap_or(databrain_connector_core::ObjectKind::Table);
            kind.is_relation().then_some(DbObject { schema, name, kind, comment, row_estimate: None })
        })
        .collect())
}

/// Completion, remote part: filtered on the server (schema-scoped when given),
/// so big catalogs are never loaded whole.
pub async fn complete_tables(state: &AppState, id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let hits = match schema {
        Some(s) => state.engine.search_schema(id, s, query, limit).await?,
        None if query.trim().is_empty() => vec![],
        None => state.engine.search_objects(id, query, limit).await?,
    };
    let _ = state.engine.workspace().meta_add_objects(id, &hits);
    Ok(hits)
}

/// Column names of a table from the knowledge index (no network).
pub fn complete_columns_local(state: &AppState, id: &str, schema: &str, name: &str) -> Result<Option<Vec<String>>> {
    Ok(state.engine.workspace().kn_column_names(id, schema, name)?)
}

pub async fn describe(
    state: &AppState,
    id: &str,
    schema: &str,
    name: &str,
) -> Result<ObjectDetail> {
    let d = state.engine.describe(id, schema, name).await?;
    if !d.columns.is_empty() {
        let _ = state.engine.workspace().meta_put_columns(id, schema, name, &d.columns);
    }
    Ok(d)
}

pub async fn table_layout(state: &AppState, id: &str, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
    state.engine.table_layout(id, schema, name).await
}

// ------------------------------------------------------------ running queries

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Span {
    /// UTF-16 offsets.
    pub start: usize,
    pub end: usize,
    pub sql: String,
}

/// Statement under the cursor (`cursor` is a UTF-16 offset).
pub fn statement_at_cursor(kind: ConnectorKind, text: &str, cursor: usize) -> Option<Span> {
    let spans = split_statements(text, kind);
    let byte = utf16_to_byte(text, cursor);
    statement_at(&spans, byte).map(|s| Span {
        start: byte_to_utf16(text, s.start),
        end: byte_to_utf16(text, s.end),
        sql: s.sql.clone(),
    })
}

pub fn run_query(state: &AppState, mut req: RunRequest) -> Result<RunResponse> {
    let text = req.sql.clone();
    // The engine works in bytes; the UI's base offset is UTF-16.
    let base_utf16 = req.base_offset;
    req.base_offset = 0;
    let mut resp = state.engine.run(req)?;
    let statements = match &mut resp {
        RunResponse::Started { statements, .. } => statements,
        RunResponse::NeedsConfirmation { statements, .. } => statements,
    };
    for s in statements.iter_mut() {
        s.start = base_utf16 + byte_to_utf16(&text, s.start);
        s.end = base_utf16 + byte_to_utf16(&text, s.end);
    }
    Ok(resp)
}

pub fn cancel_query(state: &AppState, job_id: &str) -> bool {
    state.engine.cancel(job_id)
}

pub fn close_tab(state: &AppState, tab_id: &str) {
    state.engine.close_tab(tab_id);
}

// ------------------------------------------------------------ results

pub fn result_info(state: &AppState, result_id: &str) -> Result<ResultInfo> {
    Ok(state.results().get(result_id)?.lock().info())
}

pub async fn fetch_page(
    state: &AppState,
    result_id: String,
    view: ViewSpec,
    offset: usize,
    limit: usize,
) -> Result<Page> {
    let rs = state.results().get(&result_id)?;
    blocking(move || Ok(rs.lock().page(&view, offset, limit.min(MAX_PAGE))?)).await
}

pub async fn find_in_result(
    state: &AppState,
    result_id: String,
    view: ViewSpec,
    query: String,
    columns: Option<Vec<usize>>,
    limit: usize,
) -> Result<FindResult> {
    let rs = state.results().get(&result_id)?;
    blocking(move || Ok(rs.lock().find_in(&view, &query, columns.as_deref(), limit.clamp(1, 100_000))?)).await
}

pub async fn column_stats(
    state: &AppState,
    result_id: String,
    view: ViewSpec,
    column: usize,
) -> Result<ColumnStats> {
    let rs = state.results().get(&result_id)?;
    blocking(move || Ok(rs.lock().column_stats(&view, column)?)).await
}

/// Export the view to a user-chosen file. Returns rows written.
pub async fn export_result(
    state: &AppState,
    result_id: String,
    view: ViewSpec,
    options: ExportOptions,
    path: String,
) -> Result<u64> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(invalid("Export path must be absolute"));
    }
    let rs = state.results().get(&result_id)?;
    blocking(move || {
        let (schema, batches) = {
            let mut g = rs.lock();
            (g.schema(), g.view_batches(&view, 10_000)?)
        };
        write_export(&path, schema, &batches, options)
    })
    .await
}

fn write_export(
    path: &Path,
    schema: databrain_connector_core::arrow::datatypes::SchemaRef,
    batches: &[databrain_connector_core::arrow::array::RecordBatch],
    options: ExportOptions,
) -> Result<u64> {
    let io = |e: std::io::Error| EngineError::new("internal", format!("{}: {e}", path.display()));
    // Write to a temp file in the same directory, then rename, so a failed
    // export never leaves a half-written file under the chosen name.
    let tmp = path.with_extension(format!(
        "{}.partial",
        path.extension().and_then(|e| e.to_str()).unwrap_or("tmp")
    ));
    match export_file(&tmp, schema, batches, options) {
        Ok(rows) => {
            std::fs::rename(&tmp, path).map_err(io)?;
            Ok(rows)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(EngineError::new("invalid", e.to_string()))
        }
    }
}

/// Serialize rows `[offset, offset+limit)` of the view (optionally a subset of
/// columns) for the clipboard.
pub async fn copy_rows(
    state: &AppState,
    result_id: String,
    view: ViewSpec,
    offset: usize,
    limit: usize,
    options: ExportOptions,
) -> Result<String> {
    let rs = state.results().get(&result_id)?;
    blocking(move || {
        let (schema, batch) = {
            let mut g = rs.lock();
            let (b, _) = g.view_slice(&view, offset, limit.min(MAX_COPY_ROWS))?;
            (g.schema(), b)
        };
        export_to_string(schema, [&batch], options)
            .map_err(|e| EngineError::new("internal", e.to_string()))
    })
    .await
}

pub fn release_result(state: &AppState, result_id: &str) -> bool {
    state.results().remove(result_id)
}

// ------------------------------------------------------------ saved queries, history, tabs, settings

pub fn list_saved_queries(state: &AppState, search: Option<String>) -> Result<Vec<SavedQuery>> {
    Ok(state.workspace.list_saved_queries(search.as_deref())?)
}

pub fn save_query(state: &AppState, query: SavedQuery) -> Result<SavedQuery> {
    Ok(state.workspace.save_query(query)?)
}

pub fn delete_saved_query(state: &AppState, id: &str) -> Result<()> {
    Ok(state.workspace.delete_saved_query(id)?)
}

pub fn list_folders(state: &AppState, kind: FolderKind) -> Result<Vec<Folder>> {
    Ok(state.workspace.list_folders(kind)?)
}

pub fn save_folder(state: &AppState, folder: Folder) -> Result<Folder> {
    Ok(state.workspace.save_folder(folder)?)
}

pub fn delete_folder(state: &AppState, id: &str) -> Result<()> {
    Ok(state.workspace.delete_folder(id)?)
}

pub fn list_history(state: &AppState, query: HistoryQuery) -> Result<Vec<HistoryEntry>> {
    Ok(state.workspace.list_history(&query)?)
}

pub fn clear_history(state: &AppState) -> Result<()> {
    Ok(state.workspace.clear_history()?)
}

// ------------------------------------------------------------ outputs

pub use databrain_query_engine::OutputInfo;

pub fn list_outputs(state: &AppState) -> Vec<OutputInfo> {
    state.engine.outputs().list()
}

/// Resolve `r12`, a name, `name__1`, `results.x` or a result id.
pub fn get_output(state: &AppState, reference: &str) -> Result<OutputInfo> {
    state.engine.outputs().resolve(reference).ok_or_else(|| EngineError::new("not_found", format!("output {reference} not found")))
}

/// Make sure the data is in memory (loads pinned snapshots) and return it.
pub async fn load_output(state: &AppState, reference: String) -> Result<OutputInfo> {
    let reg = state.engine.outputs().clone();
    let o = blocking(move || reg.ensure_loaded(&reference)).await?;
    state.engine.outputs_changed();
    Ok(o)
}

pub fn rename_output(state: &AppState, handle: &str, name: Option<String>) -> Result<OutputInfo> {
    let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    let o = state.engine.outputs().set_name(handle, name.as_deref())?;
    state.engine.outputs_changed();
    Ok(o)
}

/// Copy the DuckDB extensions shipped in the bundle (`bundled`) into the
/// writable app-data folder `dest` (missing or changed files only) and use it
/// as DuckDB's extension directory. Returns the number of files copied.
pub fn seed_duckdb_extensions(bundled: &Path, dest: &Path) -> usize {
    fn walk(src: &Path, dst: &Path, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(src) else { return };
        for e in rd.flatten() {
            let (s, d) = (e.path(), dst.join(e.file_name()));
            if s.is_dir() {
                walk(&s, &d, n);
            } else if s.extension().is_some_and(|x| x == "duckdb_extension") {
                let same = matches!((std::fs::metadata(&s), std::fs::metadata(&d)), (Ok(a), Ok(b)) if a.len() == b.len());
                if same {
                    continue;
                }
                let tmp = d.with_extension("duckdb_extension.tmp");
                if std::fs::create_dir_all(dst).is_ok() && std::fs::copy(&s, &tmp).is_ok() && std::fs::rename(&tmp, &d).is_ok() {
                    *n += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(bundled, dest, &mut n);
    #[cfg(feature = "duckdb")]
    databrain_connector_duckdb::set_extension_dir(Some(dest.to_string_lossy().into_owned()));
    n
}

/// Setting: where the thin Oracle driver is downloaded from (empty = default release).
pub const ORACLE_AGENT_URL_SETTING: &str = "oracle_agent_url";
/// UI event channel for the thin driver's first-use download.
pub const ORACLE_AGENT_EVENT: &str = "oracle-agent";

/// Thin Oracle driver: download folder (`<app data>/oracle-agent`), release
/// location, and download progress for the UI.
pub fn setup_oracle_agent(state: &AppState, app_dir: &Path) {
    #[cfg(feature = "oracle")]
    {
        use databrain_connector_oracle::agent;
        agent::set_dir(Some(app_dir.join("oracle-agent")));
        agent::set_base_url(state.workspace.get_setting(ORACLE_AGENT_URL_SETTING).ok().flatten().and_then(|v| v.as_str().map(str::to_string)));
        let ui = state.ui.clone();
        agent::set_progress_hook(Some(Arc::new(move |p: agent::Download| {
            ui.emit(
                ORACLE_AGENT_EVENT,
                match p {
                    agent::Download::Progress { done, total } => serde_json::json!({"type": "progress", "done": done, "total": total}),
                    agent::Download::Finished { ok } => serde_json::json!({"type": "finished", "ok": ok}),
                },
            )
        })));
    }
    #[cfg(not(feature = "oracle"))]
    let _ = (state, app_dir);
}

/// Setting with the Instant Client folder found or installed by DataBrain.
pub const ORACLE_CLIENT_SETTING: &str = "oracle_client_dir";

/// Use the saved Instant Client folder for Oracle connections without one.
pub fn apply_oracle_client_setting(state: &AppState) {
    #[cfg(feature = "oracle")]
    {
        let dir = state.workspace.get_setting(ORACLE_CLIENT_SETTING).ok().flatten().and_then(|v| v.as_str().map(str::to_string));
        databrain_connector_oracle::client::set_default_dir(dir);
    }
    #[cfg(not(feature = "oracle"))]
    let _ = state;
}

/// Is Oracle Instant Client available? (`lib_dir`: a folder to check first.)
#[cfg(feature = "oracle")]
pub async fn oracle_client_status(state: &AppState, lib_dir: Option<String>, app_dir: Option<PathBuf>) -> Result<databrain_connector_oracle::client::ClientStatus> {
    let extra: Vec<PathBuf> = app_dir.into_iter().map(|d| d.join("oracle")).collect();
    let st = blocking(move || Ok(databrain_connector_oracle::client::status(lib_dir.as_deref(), &extra))).await?;
    if st.installed {
        if let Some(d) = &st.lib_dir {
            // Remember the folder for connections that don't set one.
            if databrain_connector_oracle::client::default_dir().is_none() {
                databrain_connector_oracle::client::set_default_dir(Some(d.clone()));
                state.workspace.set_setting(ORACLE_CLIENT_SETTING, &serde_json::json!(d))?;
            }
        }
    }
    Ok(st)
}

/// UI event channel for Instant Client install progress.
pub const ORACLE_INSTALL_EVENT: &str = "oracle-install";

/// Set to stop a running Instant Client download.
#[cfg(feature = "oracle")]
static ORACLE_INSTALL_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "oracle")]
pub fn oracle_cancel_install() {
    ORACLE_INSTALL_CANCEL.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Download and install the latest Instant Client (Basic) for this computer
/// into `<app data>/oracle/instantclient_<ver>`. Progress is emitted on
/// [`ORACLE_INSTALL_EVENT`].
#[cfg(feature = "oracle")]
pub async fn oracle_install_client(state: &AppState, app_dir: PathBuf) -> Result<databrain_connector_oracle::client::ClientStatus> {
    let dest = app_dir.join("oracle");
    let ui = state.ui.clone();
    ORACLE_INSTALL_CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
    let dir = blocking(move || {
        let progress = |p: databrain_connector_oracle::client::InstallProgress| ui.emit(ORACLE_INSTALL_EVENT, serde_json::to_value(&p).unwrap_or_default());
        databrain_connector_oracle::client::install(&dest, &progress, &ORACLE_INSTALL_CANCEL).map_err(|e| {
            if e == "cancelled" { EngineError::new("cancelled", "download cancelled") } else { EngineError::new("install", e) }
        })
    })
    .await?;
    let d = dir.to_string_lossy().into_owned();
    databrain_connector_oracle::client::set_default_dir(Some(d.clone()));
    state.workspace.set_setting(ORACLE_CLIENT_SETTING, &serde_json::json!(d))?;
    oracle_client_status(state, Some(d), Some(app_dir)).await
}

/// Drop an output (data, saved snapshot and handle).
pub async fn drop_output(state: &AppState, reference: String) -> Result<OutputInfo> {
    let reg = state.engine.outputs().clone();
    let o = blocking(move || reg.remove(&reference)).await?;
    state.engine.outputs_changed();
    Ok(o)
}

/// Drop every output that is neither pinned nor a tab's latest result.
pub async fn drop_unpinned_outputs(state: &AppState) -> Result<usize> {
    let reg = state.engine.outputs().clone();
    let n = blocking(move || Ok(reg.remove_unpinned())).await?;
    state.engine.outputs_changed();
    Ok(n)
}

pub async fn pin_output(state: &AppState, handle: String, pinned: bool) -> Result<OutputInfo> {
    let reg = state.engine.outputs().clone();
    let o = blocking(move || reg.set_pinned(&handle, pinned)).await?;
    state.engine.outputs_changed();
    Ok(o)
}

/// DuckDB SQL comparing two outputs (rows added / removed / changed).
/// `mapping`: extra (before column, after column) pairs chosen by the user;
/// equal names and names equal ignoring case are matched automatically.
pub fn output_diff_sql(state: &AppState, before: &str, after: &str, keys: Vec<String>, mapping: Vec<(String, String)>) -> Result<String> {
    let b = get_output(state, before)?;
    let a = get_output(state, after)?;
    databrain_query_engine::outputs::diff_sql_mapped(&b, &a, &keys, &mapping)
}

/// Id of the local DuckDB connection for `results.*` queries.
pub fn results_connection(state: &AppState) -> Result<String> {
    Ok(state.engine.results_connection()?.id)
}

pub async fn chart_data(
    state: &AppState,
    result_id: String,
    view: ViewSpec,
    spec: databrain_result_store::ChartSpec,
) -> Result<databrain_result_store::ChartData> {
    let rs = state.results().get(&result_id)?;
    blocking(move || Ok(rs.lock().chart(&view, &spec)?)).await
}

pub fn list_notebooks(state: &AppState) -> Result<Vec<databrain_workspace::NotebookSummary>> {
    Ok(state.workspace.list_notebooks()?)
}

pub fn get_notebook(state: &AppState, id: &str) -> Result<databrain_workspace::Notebook> {
    Ok(state.workspace.get_notebook(id)?)
}

pub fn save_notebook(state: &AppState, notebook: databrain_workspace::Notebook) -> Result<databrain_workspace::Notebook> {
    Ok(state.workspace.save_notebook(notebook)?)
}

/// Deleting a notebook also releases its per-cell sessions/results.
pub fn delete_notebook(state: &AppState, id: &str) -> Result<()> {
    if let Ok(nb) = state.workspace.get_notebook(id) {
        for c in nb.cells {
            state.engine.close_tab(&format!("nb:{id}:{}", c.id));
        }
    }
    state.engine.close_tab(&format!("nb:{id}"));
    Ok(state.workspace.delete_notebook(id)?)
}

/// DuckDB SQL that reads a local file or folder (CSV, Parquet, JSON, Excel,
/// Delta, Iceberg), e.g. `SELECT * FROM read_parquet('/x/a.parquet')`.
/// Excel: the first sheet, or one statement per non-empty sheet with `all_sheets`.
#[cfg(feature = "duckdb")]
pub fn file_scan_sql(path: &str, all_sheets: bool) -> Result<String> {
    use databrain_connector_duckdb::{FileFormat, detect_format, excel_sources, file_scan_expr};
    let fmt = detect_format(path)
        .ok_or_else(|| EngineError::new("invalid", "unsupported file type (use .csv/.tsv/.parquet/.json/.ndjson/.xlsx or a Delta/Iceberg folder)"))?;
    if fmt == FileFormat::Excel {
        let srcs = excel_sources(path, all_sheets).map_err(|e| EngineError::new("invalid", e.to_string()))?;
        if srcs.is_empty() {
            return Err(EngineError::new("invalid", "every sheet in this workbook is empty"));
        }
        let many = srcs.len() > 1;
        return Ok(srcs
            .into_iter()
            .map(|(sheet, src)| {
                let label = if many { format!("-- Sheet: {sheet}\n") } else { String::new() };
                format!("{label}SELECT *\nFROM {src}\nLIMIT 1000;")
            })
            .collect::<Vec<_>>()
            .join("\n\n"));
    }
    Ok(format!("SELECT *\nFROM {}\nLIMIT 1000;", file_scan_expr(path, fmt)))
}

#[derive(Debug, Serialize)]
pub struct ExcelSheet {
    pub name: String,
    pub range: Option<String>,
    pub hidden: bool,
}

/// Sheets of an .xlsx file (workbook order) with their used ranges.
#[cfg(feature = "duckdb")]
pub fn excel_sheets(path: &str) -> Result<Vec<ExcelSheet>> {
    Ok(databrain_connector_duckdb::xlsx::sheets(path)
        .map_err(|e| EngineError::new("invalid", e))?
        .into_iter()
        .map(|s| ExcelSheet { name: s.name, range: s.range, hidden: s.hidden })
        .collect())
}

pub fn load_tabs(state: &AppState) -> Result<Vec<TabState>> {
    Ok(state.workspace.list_tabs()?)
}

pub fn save_tabs(state: &AppState, tabs: Vec<TabState>) -> Result<()> {
    Ok(state.workspace.save_tabs(&tabs)?)
}

pub fn get_settings(state: &AppState) -> Result<serde_json::Map<String, serde_json::Value>> {
    Ok(state.workspace.all_settings()?)
}

pub fn set_setting(state: &AppState, key: &str, value: serde_json::Value) -> Result<()> {
    if key.is_empty() || key.len() > 100 {
        return Err(invalid("invalid setting key"));
    }
    Ok(state.workspace.set_setting(key, &value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::{AuthMethod, MemoryStore};
    use databrain_export::ExportFormat;
    use databrain_query_engine::{CollectingSink, JobEvent};
    use databrain_result_store::{ColumnFilter, FilterOp, SortKey};
    use databrain_workspace::EnvTag;
    use std::time::Duration;

    #[test]
    fn utf16_offsets() {
        let t = "é😀a";
        assert_eq!(byte_to_utf16(t, 2), 1);
        assert_eq!(byte_to_utf16(t, 6), 3);
        assert_eq!(utf16_to_byte(t, 3), 6);
        assert_eq!(utf16_to_byte(t, 99), t.len());
        let s = statement_at_cursor(ConnectorKind::Sqlite, "select 'é';\nselect 2;", 13).unwrap();
        assert_eq!((s.start, s.end, s.sql.as_str()), (12, 20, "select 2"));
    }

    #[derive(Default)]
    pub(crate) struct TestUi(pub parking_lot::Mutex<Vec<(String, serde_json::Value)>>);
    impl crate::ai_api::UiBridge for TestUi {
        fn emit(&self, channel: &str, payload: serde_json::Value) {
            self.0.lock().push((channel.into(), payload));
        }
        fn open_url(&self, _: &str) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    fn state() -> (AppState, Arc<CollectingSink>, tempfile::TempDir) {
        let sink = Arc::new(CollectingSink::default());
        let ws = Arc::new(Workspace::open_in_memory().unwrap());
        let st = AppState::new(ws, Arc::new(MemoryStore::default()), sink.clone(), Arc::new(TestUi::default()));
        (st, sink, tempfile::tempdir().unwrap())
    }

    async fn finished_results(sink: &CollectingSink, job: &str) -> Vec<ResultInfo> {
        for _ in 0..500 {
            let evs = sink.0.lock().clone();
            if evs
                .iter()
                .any(|e| matches!(e, JobEvent::JobFinished { job_id, .. } if job_id == job))
            {
                return evs
                    .into_iter()
                    .filter_map(|e| match e {
                        JobEvent::StatementFinished { job_id, result: Some(r), .. } if job_id == job => Some(r),
                        _ => None,
                    })
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job did not finish")
    }

    /// End-to-end: create a SQLite connection, run a script, page/filter/sort/
    /// find/stats/copy/export the result, save a query and read history.
    #[tokio::test]
    async fn end_to_end_sqlite() {
        let (st, sink, dir) = state();
        let db = dir.path().join("demo.db");
        let profile = save_connection(
            &st,
            SaveConnectionArgs {
                profile: ConnectionProfile {
                    id: String::new(),
                    name: "Demo".into(),
                    config: ConnectionConfig {
                        kind: ConnectorKind::Sqlite,
                        host: None,
                        port: None,
                        database: None,
                        file_path: Some(db.to_string_lossy().into()),
                        auth: AuthMethod::None,
                        ssl_mode: Default::default(),
                        read_only: false,
                        options: Default::default(),
                        ssh: None,
                    },
                    color: None,
                    env: EnvTag::Dev,
                    folder_id: None,
                    has_secret: false,
                    ai_policy: Default::default(),
                    created_at: 0,
                    updated_at: 0,
                },
                secret: None,
                clear_secret: false,
                extra_secrets: Default::default(),
            },
        )
        .unwrap();

        let t = test_connection(&st, profile.config.clone(), None, None, None).await.unwrap();
        assert!(t.server_version.starts_with("SQLite"));

        let script = "create table people(id integer primary key, name text, city text, age integer);\n\
            insert into people(name, city, age) values ('Ann','Hanoi',31),('Binh','Hue',25),('Chi','Hanoi',42),('Dung',null,19);\n\
            select * from people;";
        let resp = run_query(
            &st,
            RunRequest {
                connection_id: profile.id.clone(),
                tab_id: "t1".into(),
                sql: script.into(),
                base_offset: 5,
                row_limit: Some(1000),
                confirmed: false,
                origin: Default::default(),
                session_key: None,
                output_name: None,
            },
        )
        .unwrap();
        let RunResponse::Started { job_id, statements } = resp else {
            panic!("expected start")
        };
        assert_eq!(statements.len(), 3);
        assert_eq!(statements[0].start, 5);
        let results = finished_results(&sink, &job_id).await;
        assert_eq!(results.len(), 1);
        let rid = results[0].id.clone();
        assert_eq!(results[0].total_rows, 4);

        let view = ViewSpec {
            filters: vec![ColumnFilter {
                column: 2,
                op: FilterOp::Equals,
                value: "Hanoi".into(),
            }],
            quick_filter: None,
            sort: vec![SortKey {
                column: 3,
                descending: true,
            }],
        };
        let page = fetch_page(&st, rid.clone(), view.clone(), 0, 100).await.unwrap();
        assert_eq!(page.view_rows, 2);
        assert_eq!(page.rows[0][1].as_deref(), Some("Chi"));

        let f = find_in_result(&st, rid.clone(), ViewSpec::default(), "han".into(), None, 100)
            .await
            .unwrap();
        assert_eq!(f.matches.len(), 2);

        let stats = column_stats(&st, rid.clone(), ViewSpec::default(), 2).await.unwrap();
        assert_eq!((stats.nulls, stats.distinct), (1, 2));

        let tsv = copy_rows(
            &st,
            rid.clone(),
            view.clone(),
            0,
            1,
            ExportOptions::new(ExportFormat::Tsv),
        )
        .await
        .unwrap();
        assert_eq!(tsv, "id\tname\tcity\tage\n3\tChi\tHanoi\t42\n");

        let out = dir.path().join("hanoi.csv");
        let n = export_result(
            &st,
            rid.clone(),
            view,
            ExportOptions::new(ExportFormat::Csv),
            out.to_string_lossy().into(),
        )
        .await
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "id,name,city,age\n3,Chi,Hanoi,42\n1,Ann,Hanoi,31\n"
        );
        assert!(export_result(&st, rid.clone(), ViewSpec::default(), ExportOptions::new(ExportFormat::Csv), "rel.csv".into())
            .await
            .is_err());

        // Explorer
        let objs = list_objects(&st, &profile.id, "main").await.unwrap();
        assert_eq!(objs[0].name, "people");
        assert!(list_connections(&st).unwrap()[0].connected);

        // Saved queries + history
        save_query(
            &st,
            SavedQuery {
                id: String::new(),
                name: "Hanoi people".into(),
                sql: "select * from people where city = 'Hanoi'".into(),
                connection_id: Some(profile.id.clone()),
                folder_id: None,
                description: None,
                tags: vec![],
                ai_example: false,
                created_at: 0,
                updated_at: 0,
            },
        )
        .unwrap();
        assert_eq!(list_saved_queries(&st, Some("hanoi".into())).unwrap().len(), 1);
        assert_eq!(list_history(&st, HistoryQuery::default()).unwrap().len(), 3);

        close_tab(&st, "t1");
        assert!(result_info(&st, &rid).is_err());
        delete_connection(&st, &profile.id).unwrap();
        assert!(list_connections(&st).unwrap().is_empty());
    }

    /// Switching to the local vault moves saved passwords; the connection
    /// keeps working and a restart (new store instance) reads the vault.
    #[tokio::test]
    async fn credential_store_switch_moves_secrets() {
        use databrain_auth::{StoreKind, SwitchableStore, VaultStore};
        use secrecy::ExposeSecret;
        let dir = tempfile::tempdir().unwrap();
        let ws = Arc::new(Workspace::open_in_memory().unwrap());
        let keychain = Arc::new(MemoryStore::default());
        struct Shared(Arc<MemoryStore>);
        impl SecretStore for Shared {
            fn get(&self, r: &SecretRef) -> std::result::Result<Option<secrecy::SecretString>, databrain_auth::AuthError> { self.0.get(r) }
            fn set(&self, r: &SecretRef, v: &secrecy::SecretString) -> std::result::Result<(), databrain_auth::AuthError> { self.0.set(r, v) }
            fn delete(&self, r: &SecretRef) -> std::result::Result<(), databrain_auth::AuthError> { self.0.delete(r) }
        }
        let store = Arc::new(SwitchableStore::with_stores(StoreKind::Keychain, Box::new(Shared(keychain.clone())), Box::new(VaultStore::new(dir.path()))));
        let st = AppState::new(ws.clone(), store.clone(), Arc::new(CollectingSink::default()), Arc::new(TestUi::default()));
        st.set_credential_store(store.clone(), dir.path().to_path_buf());
        assert_eq!(credential_store(&st).kind, StoreKind::Keychain);
        let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, databrain_auth::AuthMethod::Password { user: "me".into() });
        cfg.host = Some("localhost".into());
        let saved = save_connection(
            &st,
            SaveConnectionArgs {
                profile: ConnectionProfile { id: String::new(), name: "PG".into(), config: cfg, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 },
                secret: Some("hunter2".into()),
                clear_secret: false,
                extra_secrets: [("ssh".to_string(), "sshpw".to_string())].into_iter().collect(),
            },
        )
        .unwrap();

        let rep = set_credential_store(&st, StoreKind::Vault).await.unwrap();
        assert_eq!((rep.moved, rep.failed.len()), (2, 0));
        assert_eq!(credential_store(&st).kind, StoreKind::Vault);
        assert_eq!(saved_store_kind(&ws), StoreKind::Vault);
        assert!(keychain.get(&SecretRef::for_connection(&saved.id)).unwrap().is_none(), "removed from the keychain");
        assert_eq!(st.secrets.get(&SecretRef::for_connection(&saved.id)).unwrap().unwrap().expose_secret(), "hunter2");
        // "Restart": a new vault store reads the same files.
        let again = SwitchableStore::with_stores(saved_store_kind(&ws), Box::new(MemoryStore::default()), Box::new(VaultStore::new(dir.path())));
        assert_eq!(again.get(&SecretRef::slot(&saved.id, "ssh")).unwrap().unwrap().expose_secret(), "sshpw");
    }

    #[test]
    fn seeds_bundled_duckdb_extensions() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let plat = src.path().join("v1.5.6/osx_arm64");
        std::fs::create_dir_all(&plat).unwrap();
        std::fs::write(plat.join("excel.duckdb_extension"), b"bin").unwrap();
        std::fs::write(src.path().join("README.md"), b"x").unwrap();
        assert_eq!(seed_duckdb_extensions(src.path(), dst.path()), 1);
        assert!(dst.path().join("v1.5.6/osx_arm64/excel.duckdb_extension").is_file());
        assert!(!dst.path().join("README.md").exists());
        assert_eq!(seed_duckdb_extensions(src.path(), dst.path()), 0, "unchanged files are not copied again");
        #[cfg(feature = "duckdb")]
        assert_eq!(databrain_connector_duckdb::extension_dir().as_deref(), Some(&*dst.path().to_string_lossy()));
    }

    /// Deleting removes the connection, all its secrets and AI knowledge;
    /// saved queries survive with no connection.
    #[test]
    fn delete_connection_cleans_up() {
        let (st, _sink, _dir) = state();
        let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: "me".into() });
        cfg.host = Some("db".into());
        let saved = save_connection(
            &st,
            SaveConnectionArgs {
                profile: ConnectionProfile { id: String::new(), name: "PG".into(), config: cfg, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 },
                secret: Some("pw".into()),
                clear_secret: false,
                extra_secrets: [("ssh".to_string(), "sshpw".to_string()), ("client_secret".to_string(), "cs".to_string())].into_iter().collect(),
            },
        )
        .unwrap();
        let id = saved.id.clone();
        st.secrets.set(&SecretRef::slot(&id, "oauth"), &"tokens".to_string().into()).unwrap();
        st.workspace
            .kn_save_note(databrain_workspace::KnNote { id: String::new(), connection_id: id.clone(), target: None, body: "rule".into(), author: "user".into(), status: databrain_workspace::NoteStatus::Approved, created_at: 0, replaces: None })
            .unwrap();
        let q = save_query(&st, SavedQuery { id: String::new(), name: "q".into(), sql: "select 1".into(), connection_id: Some(id.clone()), folder_id: None, description: None, tags: vec![], ai_example: false, created_at: 0, updated_at: 0 }).unwrap();

        delete_connection(&st, &id).unwrap();
        assert!(list_connections(&st).unwrap().is_empty());
        for r in [SecretRef::for_connection(&id), SecretRef::slot(&id, "ssh"), SecretRef::slot(&id, "client_secret"), SecretRef::slot(&id, "oauth")] {
            assert!(st.secrets.get(&r).unwrap().is_none(), "{r:?} left behind");
        }
        assert!(st.workspace.kn_notes(&id).unwrap().is_empty());
        let kept = list_saved_queries(&st, None).unwrap();
        assert_eq!(kept.iter().find(|x| x.id == q.id).unwrap().connection_id, None);
        assert!(delete_connection(&st, &id).is_err(), "unknown id");
    }

    #[test]
    fn secrets_are_stored_in_secret_store_only() {
        let (st, _sink, _dir) = state();
        let saved = save_connection(
            &st,
            SaveConnectionArgs {
                profile: ConnectionProfile {
                    id: String::new(),
                    name: "PG".into(),
                    config: ConnectionConfig {
                        kind: ConnectorKind::Postgres,
                        host: Some("localhost".into()),
                        port: Some(5432),
                        database: Some("postgres".into()),
                        file_path: None,
                        auth: AuthMethod::Password { user: "me".into() },
                        ssl_mode: Default::default(),
                        read_only: false,
                        options: Default::default(),
                        ssh: None,
                    },
                    color: None,
                    env: EnvTag::None,
                    folder_id: None,
                    has_secret: false,
                    ai_policy: Default::default(),
                    created_at: 0,
                    updated_at: 0,
                },
                secret: Some("hunter2".into()),
                clear_secret: false,
                extra_secrets: [("ssh".to_string(), "sshpw".to_string()), ("bogus".to_string(), "x".to_string())].into_iter().collect(),
            },
        )
        .unwrap();
        assert!(saved.has_secret);
        let json = serde_json::to_string(&st.workspace.get_connection(&saved.id).unwrap()).unwrap();
        assert!(!json.contains("hunter2"));
        let r = SecretRef::for_connection(&saved.id);
        use secrecy::ExposeSecret;
        assert_eq!(st.secrets.get(&r).unwrap().unwrap().expose_secret(), "hunter2");
        assert_eq!(st.secrets.get(&SecretRef::slot(&saved.id, "ssh")).unwrap().unwrap().expose_secret(), "sshpw");
        // Folder move
        let f = save_folder(&st, Folder { id: String::new(), parent_id: None, name: "Prod".into(), kind: FolderKind::Connections }).unwrap();
        move_to_folder(&st, FolderKind::Connections, &saved.id, Some(f.id.clone())).unwrap();
        assert_eq!(st.workspace.get_connection(&saved.id).unwrap().folder_id, Some(f.id));
        #[cfg(feature = "duckdb")]
        {
            assert_eq!(file_scan_sql("/d/x.csv", false).unwrap(), "SELECT *\nFROM read_csv('/d/x.csv')\nLIMIT 1000;");
            assert!(file_scan_sql("/d/x.exe", false).is_err());
        }

        // Editing without a secret keeps it; clearing removes it.
        let mut p = saved.clone();
        p.has_secret = false;
        let kept = save_connection(&st, SaveConnectionArgs { profile: p.clone(), secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap();
        assert!(kept.has_secret);
        let cleared = save_connection(&st, SaveConnectionArgs { profile: p, secret: None, clear_secret: true, extra_secrets: [("ssh".to_string(), String::new())].into_iter().collect() }).unwrap();
        assert!(!cleared.has_secret);
        assert!(st.secrets.get(&r).unwrap().is_none());
        assert!(st.secrets.get(&SecretRef::slot(&saved.id, "ssh")).unwrap().is_none());
        assert!(st.secrets.get(&SecretRef::slot(&saved.id, "bogus")).unwrap().is_none());
    }
}
