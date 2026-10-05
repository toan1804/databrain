//! Query orchestration: per-tab sessions, jobs that run scripts statement by
//! statement, row caps, cancellation, write-safety confirmation and history.
//!
//! UI-facing progress is reported through an [`EventSink`] so the engine has
//! no dependency on Tauri.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub mod outputs;

pub use outputs::{NewOutput, OutputInfo, OutputRegistry, OutputState, OutputTables};

use databrain_auth::{
    AuthStatus, CredentialSource, ExposeSecret, InlineCredentialSource, Interaction, NonInteractive, SecretRef, SecretStore,
    credential_source,
};
use databrain_ssh_tunnel::{SshSecret, Tunnel, TunnelSpec};
use databrain_connector_core::sql::{Classification, StatementKind, classify, split_statements};
use databrain_connector_core::{
    CancellationToken, ConnectionConfig, ConnectorError, ConnectorRegistry, DbObject, ErrorKind,
    ExecOptions, ObjectDetail, SchemaInfo, Session, StreamEvent, TableLayout,
};
use databrain_result_store::{ResultInfo, ResultStore};
use databrain_workspace::{ConnectionProfile, EnvTag, NewHistory, Origin, RunStatus, Workspace, now_ms};
use parking_lot::Mutex;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------ errors

/// Serializable error returned to the UI.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct EngineError {
    /// Machine-readable category: `auth`, `connection`, `query`, `cancelled`,
    /// `unsupported`, `config`, `internal`, `not_found`, `invalid`, `busy`.
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// 1-based character position of the error within the statement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
}

impl EngineError {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            code: None,
            position: None,
        }
    }
}

impl From<ConnectorError> for EngineError {
    fn from(e: ConnectorError) -> Self {
        let kind = serde_json::to_value(e.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "internal".into());
        Self {
            kind,
            message: e.message,
            code: e.code,
            position: e.position,
        }
    }
}

impl From<databrain_workspace::Error> for EngineError {
    fn from(e: databrain_workspace::Error) -> Self {
        let kind = match e {
            databrain_workspace::Error::NotFound(_) => "not_found",
            databrain_workspace::Error::Invalid(_) => "invalid",
            _ => "internal",
        };
        Self::new(kind, e.to_string())
    }
}

impl From<databrain_result_store::Error> for EngineError {
    fn from(e: databrain_result_store::Error) -> Self {
        let kind = match e {
            databrain_result_store::Error::NotFound(_) => "not_found",
            databrain_result_store::Error::InvalidColumn(_)
            | databrain_result_store::Error::InvalidFilterValue(_) => "invalid",
            _ => "internal",
        };
        Self::new(kind, e.to_string())
    }
}

impl From<databrain_auth::AuthError> for EngineError {
    fn from(e: databrain_auth::AuthError) -> Self {
        match e {
            databrain_auth::AuthError::ReauthRequired => Self::new("reauth_required", "Sign in to this connection first"),
            databrain_auth::AuthError::Cancelled => Self::new("cancelled", "Sign-in cancelled"),
            other => Self::new("auth", other.to_string()),
        }
    }
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

// ------------------------------------------------------------------ events

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    StatementStarted {
        job_id: String,
        tab_id: String,
        index: usize,
        sql: String,
    },
    Progress {
        job_id: String,
        tab_id: String,
        index: usize,
        rows: usize,
        elapsed_ms: u64,
    },
    StatementFinished {
        job_id: String,
        tab_id: String,
        index: usize,
        result: Option<ResultInfo>,
        /// Handle/name/provenance of the result (when there is one).
        #[serde(default)]
        output: Option<Box<OutputInfo>>,
        rows_affected: Option<u64>,
        duration_ms: u64,
        notices: Vec<String>,
    },
    StatementFailed {
        job_id: String,
        tab_id: String,
        index: usize,
        error: EngineError,
        duration_ms: u64,
        /// Byte offset of the statement within the submitted text, so the
        /// editor can map `error.position` to a location.
        statement_start: usize,
    },
    JobFinished {
        job_id: String,
        tab_id: String,
        status: RunStatus,
        duration_ms: u64,
    },
    /// Outputs were added, renamed, pinned or evicted (job/tab ids empty).
    OutputsChanged { job_id: String, tab_id: String },
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: JobEvent);
}

/// Fans events out to a primary sink (the UI) and in-process subscribers
/// (the AI agent and MCP server wait for their jobs through this).
pub struct EventHub {
    primary: Arc<dyn EventSink>,
    tx: tokio::sync::broadcast::Sender<JobEvent>,
}

impl EventHub {
    pub fn new(primary: Arc<dyn EventSink>) -> Arc<Self> {
        let (tx, _) = tokio::sync::broadcast::channel(1024);
        Arc::new(Self { primary, tx })
    }
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<JobEvent> {
        self.tx.subscribe()
    }
}

impl EventSink for EventHub {
    fn emit(&self, event: JobEvent) {
        let _ = self.tx.send(event.clone());
        self.primary.emit(event);
    }
}

/// Outcome of one statement for programmatic callers.
#[derive(Debug, Clone, Serialize)]
pub struct StatementOutcomeView {
    pub index: usize,
    pub sql: String,
    pub result: Option<ResultInfo>,
    pub rows_affected: Option<u64>,
    pub error: Option<EngineError>,
    pub duration_ms: u64,
    pub notices: Vec<String>,
}

impl JobEvent {
    pub fn job_id(&self) -> &str {
        match self {
            JobEvent::StatementStarted { job_id, .. }
            | JobEvent::Progress { job_id, .. }
            | JobEvent::StatementFinished { job_id, .. }
            | JobEvent::StatementFailed { job_id, .. }
            | JobEvent::JobFinished { job_id, .. }
            | JobEvent::OutputsChanged { job_id, .. } => job_id,
        }
    }
}

/// Event sink that collects events (tests).
#[derive(Default)]
pub struct CollectingSink(pub Mutex<Vec<JobEvent>>);

impl EventSink for CollectingSink {
    fn emit(&self, event: JobEvent) {
        self.0.lock().push(event);
    }
}

// ------------------------------------------------------------------ requests

#[derive(Debug, Clone, Deserialize)]
pub struct RunRequest {
    pub connection_id: String,
    pub tab_id: String,
    /// Full editor text or selection to run.
    pub sql: String,
    /// Offset of `sql` within the editor document (for error locations).
    #[serde(default)]
    pub base_offset: usize,
    /// Max rows kept per result; `None` or 0 = unlimited.
    #[serde(default)]
    pub row_limit: Option<usize>,
    /// Set after the user confirmed a write-safety prompt.
    #[serde(default)]
    pub confirmed: bool,
    /// Who submitted the query (recorded in history).
    #[serde(default)]
    pub origin: Origin,
    /// Share one database session between several run keys (notebook
    /// cells: each cell keeps its own results, all cells share temp tables
    /// and session settings). Defaults to `tab_id`.
    #[serde(default)]
    pub session_key: Option<String>,
    /// Name the job's last result (`results.<name>` in DuckDB sessions).
    #[serde(default)]
    pub output_name: Option<String>,
}

impl RunRequest {
    pub fn session_key(&self) -> &str {
        self.session_key.as_deref().filter(|k| !k.is_empty()).unwrap_or(&self.tab_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedStatement {
    pub index: usize,
    pub sql: String,
    pub start: usize,
    pub end: usize,
    pub classification: Classification,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunResponse {
    Started {
        job_id: String,
        statements: Vec<PlannedStatement>,
    },
    /// Nothing ran; the UI must ask the user and resubmit with `confirmed`.
    NeedsConfirmation {
        reasons: Vec<String>,
        statements: Vec<PlannedStatement>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub server_version: String,
    pub latency_ms: u64,
}

// ------------------------------------------------------------------ engine

struct JobHandle {
    tab_id: String,
    session_key: String,
    cancel: CancellationToken,
}

type SessionKey = (String, String); // (connection_id, tab_id)

const META_TAB: &str = "__meta__";
/// Connection option marking the "Results (DuckDB)" connection.
pub const RESULTS_MARKER: &str = "databrain_results";

const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

/// A live session plus the SSH tunnel it runs through (kept alive together).
struct Live {
    session: Arc<dyn Session>,
    _tunnel: Option<Arc<Tunnel>>,
}

pub struct QueryEngine {
    registry: ConnectorRegistry,
    workspace: Arc<Workspace>,
    secrets: Arc<dyn SecretStore>,
    results: Arc<ResultStore>,
    events: Arc<dyn EventSink>,
    interaction: Mutex<Arc<dyn Interaction>>,
    /// Credential sources per connection (token caches live here).
    creds: Mutex<HashMap<String, Arc<dyn CredentialSource>>>,
    sessions: Mutex<HashMap<SessionKey, Live>>,
    /// Serializes connection attempts per key so parallel calls share one session.
    connecting: Mutex<HashMap<SessionKey, Arc<tokio::sync::Mutex<()>>>>,
    jobs: Mutex<HashMap<String, JobHandle>>,
    outputs: Arc<OutputRegistry>,
}

impl QueryEngine {
    pub fn new(
        registry: ConnectorRegistry,
        workspace: Arc<Workspace>,
        secrets: Arc<dyn SecretStore>,
        results: Arc<ResultStore>,
        events: Arc<dyn EventSink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            workspace: workspace.clone(),
            secrets,
            events,
            interaction: Mutex::new(Arc::new(NonInteractive)),
            creds: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            outputs: OutputRegistry::new(results.clone(), workspace.clone()),
            results,
        })
    }

    /// Use `interaction` for browser / device-code sign-in prompts.
    pub fn set_interaction(&self, interaction: Arc<dyn Interaction>) {
        *self.interaction.lock() = interaction;
        self.creds.lock().clear();
    }

    fn interaction(&self) -> Arc<dyn Interaction> {
        self.interaction.lock().clone()
    }

    pub fn registry(&self) -> &ConnectorRegistry {
        &self.registry
    }

    pub fn workspace(&self) -> &Arc<Workspace> {
        &self.workspace
    }

    pub fn secrets(&self) -> &Arc<dyn SecretStore> {
        &self.secrets
    }

    /// Credential source for a saved connection (cached so tokens persist
    /// across sessions and tabs).
    pub fn credentials(&self, profile: &ConnectionProfile) -> Result<Arc<dyn CredentialSource>> {
        if let Some(c) = self.creds.lock().get(&profile.id) {
            return Ok(c.clone());
        }
        let connector = self.registry.get(profile.config.kind)?;
        let src = credential_source(
            &profile.config.auth,
            &profile.id,
            self.secrets.clone(),
            self.interaction(),
            connector.auth_context(&profile.config),
        )?;
        self.creds.lock().insert(profile.id.clone(), src.clone());
        Ok(src)
    }

    /// Sign in now (browser / device code / token exchange).
    pub async fn sign_in(&self, connection_id: &str) -> Result<AuthStatus> {
        let profile = self.workspace.get_connection(connection_id)?;
        Ok(self.credentials(&profile)?.sign_in().await?)
    }

    pub async fn sign_out(&self, connection_id: &str) -> Result<()> {
        let profile = self.workspace.get_connection(connection_id)?;
        self.credentials(&profile)?.sign_out().await?;
        self.disconnect(connection_id);
        Ok(())
    }

    pub fn auth_status(&self, connection_id: &str) -> Result<AuthStatus> {
        let profile = self.workspace.get_connection(connection_id)?;
        Ok(self.credentials(&profile)?.status())
    }

    /// Forget cached credentials after a connection is edited.
    pub fn forget_credentials(&self, connection_id: &str) {
        self.creds.lock().remove(connection_id);
    }

    /// Open the SSH tunnel for a config (if configured) and return the
    /// config rewritten to go through it.
    async fn tunnel(&self, cfg: &ConnectionConfig, owner_id: Option<&str>, secret_override: Option<&str>) -> Result<(ConnectionConfig, Option<Arc<Tunnel>>)> {
        let Some(ssh) = cfg.ssh.clone() else { return Ok((cfg.clone(), None)) };
        let stored = match owner_id {
            Some(id) => self.secrets.get(&SecretRef::slot(id, "ssh"))?.map(|s| s.expose_secret().to_string()),
            None => None,
        };
        let secret_text = secret_override.map(str::to_string).or(stored);
        let secret = match (&ssh.auth, secret_text) {
            (databrain_connector_core::SshAuth::Password, Some(p)) => SshSecret::Password(p),
            (databrain_connector_core::SshAuth::Key { .. }, Some(p)) => SshSecret::KeyPassphrase(p),
            _ => SshSecret::None,
        };
        let default_port = self.registry.get(cfg.kind)?.info().default_port.unwrap_or(0);
        let target_port = cfg.port.unwrap_or(default_port);
        let t = Tunnel::open(TunnelSpec {
            ssh_host: &ssh.host,
            ssh_port: ssh.port,
            user: &ssh.user,
            auth: &ssh.auth,
            secret,
            expected_fingerprint: ssh.host_key_fingerprint.as_deref(),
            target_host: cfg.host_or_default(),
            target_port,
        })
        .await
        .map_err(|e| EngineError::new("connection", e.to_string()))?;
        // Trust on first use: pin the server key.
        if ssh.host_key_fingerprint.is_none() {
            if let Some(id) = owner_id {
                if let Ok(mut p) = self.workspace.get_connection(id) {
                    if let Some(s) = p.config.ssh.as_mut() {
                        s.host_key_fingerprint = Some(t.fingerprint().to_string());
                        let _ = self.workspace.save_connection(p);
                    }
                }
            }
        }
        let mut rewritten = cfg.clone();
        rewritten.host = Some("127.0.0.1".into());
        rewritten.port = Some(t.local_port());
        Ok((rewritten, Some(Arc::new(t))))
    }

    pub fn outputs(&self) -> &Arc<OutputRegistry> {
        &self.outputs
    }

    /// For DuckDB sessions (`results.<name>`).
    pub fn external_tables(&self) -> Arc<dyn databrain_connector_core::external::ExternalTables> {
        Arc::new(OutputTables(self.outputs.clone()))
    }

    /// The local DuckDB connection used to query outputs together
    /// (`results.<name>`), created on first use. Marked with the option
    /// `databrain_results = 1`.
    pub fn results_connection(&self) -> Result<ConnectionProfile> {
        let existing = self
            .workspace
            .list_connections()?
            .into_iter()
            .find(|c| c.config.kind == databrain_connector_core::ConnectorKind::Duckdb && c.config.opt(RESULTS_MARKER) == Some("1"));
        if let Some(c) = existing {
            return Ok(c);
        }
        let mut config = ConnectionConfig::new(databrain_connector_core::ConnectorKind::Duckdb, databrain_auth::AuthMethod::None);
        config.options.insert(RESULTS_MARKER.into(), "1".into());
        Ok(self.workspace.save_connection(ConnectionProfile {
            id: String::new(),
            name: "Results (DuckDB)".into(),
            config,
            color: Some("#a78bfa".into()),
            env: EnvTag::None,
            folder_id: None,
            has_secret: false,
            ai_policy: Default::default(),
            created_at: 0,
            updated_at: 0,
        })?)
    }

    pub fn outputs_changed(&self) {
        self.events.emit(JobEvent::OutputsChanged { job_id: String::new(), tab_id: String::new() });
    }

    pub fn results(&self) -> &Arc<ResultStore> {
        &self.results
    }

    // -------------------------------------------------------------- sessions

    /// Test a configuration without saving it. `secret` overrides the stored
    /// secret; if `None` and `connection_id` is given, the stored one is used.
    pub async fn test_connection(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<String>,
        connection_id: Option<&str>,
        ssh_secret: Option<String>,
    ) -> Result<TestResult> {
        let connector = self.registry.get(cfg.kind)?;
        let creds: Arc<dyn CredentialSource> = match (secret, connection_id) {
            (Some(s), _) => Arc::new(InlineCredentialSource::new(cfg.auth.clone(), Some(SecretString::from(s)))),
            (None, Some(id)) => credential_source(
                &cfg.auth,
                id,
                self.secrets.clone(),
                self.interaction(),
                connector.auth_context(cfg),
            )?,
            (None, None) if cfg.auth.is_interactive() => credential_source(
                &cfg.auth,
                "__test__",
                Arc::new(databrain_auth::MemoryStore::default()),
                self.interaction(),
                connector.auth_context(cfg),
            )?,
            (None, None) => Arc::new(InlineCredentialSource::new(cfg.auth.clone(), None)),
        };
        let start = Instant::now();
        let (cfg, _tunnel) = self.tunnel(cfg, connection_id, ssh_secret.as_deref()).await?;
        let session = connector.connect(&cfg, creds).await?;
        let server_version = session.server_version().await?;
        Ok(TestResult {
            server_version,
            latency_ms: start.elapsed().as_millis() as u64,
        })
    }

    async fn session(&self, connection_id: &str, tab_id: &str) -> Result<Arc<dyn Session>> {
        let key = (connection_id.to_string(), tab_id.to_string());
        if let Some(s) = self.sessions.lock().get(&key) {
            return Ok(s.session.clone());
        }
        let gate = self
            .connecting
            .lock()
            .entry(key.clone())
            .or_default()
            .clone();
        let _g = gate.lock().await;
        if let Some(s) = self.sessions.lock().get(&key) {
            return Ok(s.session.clone());
        }
        let profile = self.workspace.get_connection(connection_id)?;
        let connector = self.registry.get(profile.config.kind)?;
        let creds = self.credentials(&profile)?;
        let (cfg, tunnel) = self.tunnel(&profile.config, Some(&profile.id), None).await?;
        let session: Arc<dyn Session> = match connector.connect(&cfg, creds.clone()).await {
            Ok(s) => Arc::from(s),
            Err(e) if e.kind == ErrorKind::Auth => {
                // Expired/revoked token: refresh once and retry.
                creds.invalidate().await;
                Arc::from(connector.connect(&cfg, creds).await?)
            }
            Err(e) => return Err(e.into()),
        };
        self.sessions.lock().insert(key, Live { session: session.clone(), _tunnel: tunnel });
        Ok(session)
    }

    fn drop_session(&self, connection_id: &str, tab_id: &str) {
        self.sessions
            .lock()
            .remove(&(connection_id.to_string(), tab_id.to_string()));
    }

    /// Close every session of a connection (after editing or deleting it).
    pub fn disconnect(&self, connection_id: &str) {
        self.sessions.lock().retain(|(c, _), _| c != connection_id);
    }

    /// Connection ids that currently have at least one open session.
    pub fn connected_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sessions.lock().keys().map(|(c, _)| c.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Release the session and results that belong to a closed tab.
    pub fn close_tab(&self, tab_id: &str) {
        let running: Vec<String> = self
            .jobs
            .lock()
            .iter()
            .filter(|(_, j)| j.tab_id == tab_id || j.session_key == tab_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in running {
            self.cancel(&id);
        }
        self.sessions.lock().retain(|(_, t), _| t != tab_id);
        self.outputs.close_tab(tab_id);
        self.outputs_changed();
    }

    // -------------------------------------------------------------- metadata

    async fn with_meta<T, F, Fut>(&self, connection_id: &str, f: F) -> Result<T>
    where
        F: Fn(Arc<dyn Session>) -> Fut,
        Fut: std::future::Future<Output = databrain_connector_core::Result<T>>,
    {
        let s = self.session(connection_id, META_TAB).await?;
        match f(s).await {
            Ok(v) => Ok(v),
            Err(e) if e.kind == ErrorKind::Connection => {
                // Stale connection: reconnect once.
                self.drop_session(connection_id, META_TAB);
                let s = self.session(connection_id, META_TAB).await?;
                Ok(f(s).await?)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Connect (if needed) and return the server version.
    pub async fn connect(&self, connection_id: &str) -> Result<String> {
        self.with_meta(connection_id, |s| async move { s.server_version().await })
            .await
    }

    pub async fn list_schemas(&self, connection_id: &str) -> Result<Vec<SchemaInfo>> {
        self.with_meta(connection_id, |s| async move { s.list_schemas().await })
            .await
    }

    pub async fn list_objects(&self, connection_id: &str, schema: &str) -> Result<Vec<DbObject>> {
        let schema = schema.to_string();
        self.with_meta(connection_id, |s| {
            let schema = schema.clone();
            async move { s.list_objects(&schema).await }
        })
        .await
    }

    /// Find tables/views by name across the connection's schemas.
    pub async fn search_objects(&self, connection_id: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let query = query.to_string();
        let limit = limit.clamp(1, 500);
        self.with_meta(connection_id, |s| {
            let query = query.clone();
            async move { s.search_objects(&query, limit).await }
        })
        .await
    }

    /// Tables/views of one schema matching `query` (editor completion).
    pub async fn search_schema(&self, connection_id: &str, schema: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let (schema, query) = (schema.to_string(), query.to_string());
        let limit = limit.clamp(1, 1000);
        self.with_meta(connection_id, |s| {
            let (schema, query) = (schema.clone(), query.clone());
            async move { s.search_schema(&schema, &query, limit).await }
        })
        .await
    }

    pub async fn bulk_metadata(&self, connection_id: &str, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        let schemas = schemas.to_vec();
        self.with_meta(connection_id, |s| {
            let schemas = schemas.clone();
            async move { s.bulk_metadata(&schemas).await }
        })
        .await
    }

    pub async fn schema_fingerprints(&self, connection_id: &str) -> Result<Option<std::collections::HashMap<String, String>>> {
        self.with_meta(connection_id, |s| async move { s.schema_fingerprints().await }).await
    }

    pub async fn schema_object_counts(&self, connection_id: &str) -> Result<Option<std::collections::HashMap<String, usize>>> {
        self.with_meta(connection_id, |s| async move { s.schema_object_counts().await }).await
    }

    pub async fn schema_columns(&self, connection_id: &str, schema: &str) -> Result<Vec<databrain_connector_core::TableColumns>> {
        let schema = schema.to_string();
        self.with_meta(connection_id, |s| {
            let schema = schema.clone();
            async move { s.schema_columns(&schema).await }
        })
        .await
    }

    pub async fn describe(
        &self,
        connection_id: &str,
        schema: &str,
        name: &str,
    ) -> Result<ObjectDetail> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.with_meta(connection_id, |s| {
            let (schema, name) = (schema.clone(), name.clone());
            async move { s.describe(&schema, &name).await }
        })
        .await
    }

    /// Indexes, partitioning and clustering of a table (editor query hints).
    pub async fn table_layout(&self, connection_id: &str, schema: &str, name: &str) -> Result<TableLayout> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.with_meta(connection_id, |s| {
            let (schema, name) = (schema.clone(), name.clone());
            async move { s.table_layout(&schema, &name).await }
        })
        .await
    }

    // -------------------------------------------------------------- jobs

    pub fn plan(&self, profile: &ConnectionProfile, sql: &str) -> Vec<PlannedStatement> {
        let kind = profile.config.kind;
        split_statements(sql, kind)
            .into_iter()
            .enumerate()
            .map(|(index, s)| PlannedStatement {
                index,
                classification: classify(&s.sql, kind),
                sql: s.sql,
                start: s.start,
                end: s.end,
            })
            .collect()
    }

    /// Reasons the user must confirm before running, if any.
    fn safety_reasons(profile: &ConnectionProfile, plan: &[PlannedStatement]) -> Vec<String> {
        let mut reasons = Vec::new();
        for p in plan {
            let c = &p.classification;
            if c.missing_where {
                reasons.push(format!(
                    "Statement {} is {} without a WHERE clause and will affect every row.",
                    p.index + 1,
                    c.keyword
                ));
            }
        }
        if profile.env == EnvTag::Prod {
            let writes = plan
                .iter()
                .filter(|p| p.classification.kind.is_write() || p.classification.kind == StatementKind::Unknown)
                .count();
            if writes > 0 {
                reasons.push(format!(
                    "{writes} statement(s) may modify data or schema on the production connection \"{}\".",
                    profile.name
                ));
            }
        }
        reasons
    }

    pub fn run(self: &Arc<Self>, req: RunRequest) -> Result<RunResponse> {
        let profile = self.workspace.get_connection(&req.connection_id)?;
        let plan = self.plan(&profile, &req.sql);
        if plan.is_empty() {
            return Err(EngineError::new("invalid", "Nothing to run"));
        }
        if profile.config.read_only {
            if let Some(p) = plan.iter().find(|p| p.classification.kind.is_write()) {
                return Err(EngineError::new(
                    "invalid",
                    format!(
                        "Connection \"{}\" is read-only; statement {} ({}) was not run.",
                        profile.name,
                        p.index + 1,
                        p.classification.keyword
                    ),
                ));
            }
        }
        if !req.confirmed {
            let reasons = Self::safety_reasons(&profile, &plan);
            if !reasons.is_empty() {
                return Ok(RunResponse::NeedsConfirmation {
                    reasons,
                    statements: plan,
                });
            }
        }
        if let Some(n) = req.output_name.as_deref().filter(|n| !n.is_empty()) {
            outputs::validate_name(n)?;
        }
        if self.jobs.lock().values().any(|j| j.tab_id == req.tab_id || j.session_key == req.session_key()) {
            return Err(EngineError::new(
                "busy",
                "A query is already running in this tab",
            ));
        }

        let job_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        self.jobs.lock().insert(
            job_id.clone(),
            JobHandle {
                tab_id: req.tab_id.clone(),
                session_key: req.session_key().to_string(),
                cancel: cancel.clone(),
            },
        );
        // Previous outputs of this tab become "recent" (evictable).
        self.outputs.begin_run(&req.tab_id);

        let engine = self.clone();
        let jid = job_id.clone();
        let statements = plan.clone();
        tokio::spawn(async move {
            engine.run_job(jid, req, statements, cancel).await;
        });
        Ok(RunResponse::Started {
            job_id,
            statements: plan,
        })
    }

    /// Run and wait for completion (for the AI agent / MCP). Requires an
    /// [`EventHub`]; `confirmed` skips the interactive safety prompt because
    /// the caller has already enforced its own policy.
    pub async fn run_and_wait(
        self: &Arc<Self>,
        hub: &EventHub,
        req: RunRequest,
        cancel: Option<CancellationToken>,
    ) -> Result<Vec<StatementOutcomeView>> {
        let mut rx = hub.subscribe();
        let statements = match self.run(req)? {
            RunResponse::Started { job_id, statements } => (job_id, statements),
            RunResponse::NeedsConfirmation { reasons, .. } => {
                return Err(EngineError::new("invalid", reasons.join(" ")));
            }
        };
        let (job_id, plan) = statements;
        let mut out: Vec<StatementOutcomeView> = plan
            .iter()
            .map(|p| StatementOutcomeView { index: p.index, sql: p.sql.clone(), result: None, rows_affected: None, error: None, duration_ms: 0, notices: vec![] })
            .collect();
        let cancel = cancel.unwrap_or_default();
        loop {
            let ev = tokio::select! {
                ev = rx.recv() => ev,
                _ = cancel.cancelled() => {
                    self.cancel(&job_id);
                    continue;
                }
            };
            let ev = match ev {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return Err(EngineError::new("internal", "event channel closed")),
            };
            if ev.job_id() != job_id {
                continue;
            }
            match ev {
                JobEvent::StatementFinished { index, result, rows_affected, duration_ms, notices, .. } => {
                    if let Some(o) = out.get_mut(index) {
                        o.result = result;
                        o.rows_affected = rows_affected;
                        o.duration_ms = duration_ms;
                        o.notices = notices;
                    }
                }
                JobEvent::StatementFailed { index, error, duration_ms, .. } => {
                    if let Some(o) = out.get_mut(index) {
                        o.error = Some(error);
                        o.duration_ms = duration_ms;
                    }
                }
                JobEvent::JobFinished { .. } => return Ok(out),
                _ => {}
            }
        }
    }

    pub fn cancel(&self, job_id: &str) -> bool {
        match self.jobs.lock().get(job_id) {
            Some(j) => {
                j.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub fn is_running(&self, job_id: &str) -> bool {
        self.jobs.lock().contains_key(job_id)
    }

    async fn run_job(
        self: Arc<Self>,
        job_id: String,
        req: RunRequest,
        plan: Vec<PlannedStatement>,
        cancel: CancellationToken,
    ) {
        let job_start = Instant::now();
        let mut status = RunStatus::Success;
        let limit = req.row_limit.filter(|l| *l > 0);
        let (conn_name, conn_kind) = self
            .workspace
            .get_connection(&req.connection_id)
            .map(|p| (p.name, p.config.kind))
            .unwrap_or_else(|_| (req.connection_id.clone(), databrain_connector_core::ConnectorKind::Sqlite));
        let mut last_output: Option<String> = None;

        for stmt in &plan {
            if cancel.is_cancelled() {
                status = RunStatus::Cancelled;
                break;
            }
            self.events.emit(JobEvent::StatementStarted {
                job_id: job_id.clone(),
                tab_id: req.tab_id.clone(),
                index: stmt.index,
                sql: stmt.sql.clone(),
            });
            let started_at = now_ms();
            let t0 = Instant::now();
            let outcome = self
                .run_statement(&job_id, &req, stmt, limit, cancel.child_token())
                .await;
            let duration_ms = t0.elapsed().as_millis() as u64;

            let output = match &outcome {
                Ok(StatementOutcome { result: Some(r), .. }) => Some(Box::new(self.outputs.add(NewOutput {
                    result: r.clone(),
                    connection_id: req.connection_id.clone(),
                    connection_name: conn_name.clone(),
                    kind: conn_kind,
                    sql: stmt.sql.clone(),
                    tab_id: req.tab_id.clone(),
                    statement_index: stmt.index,
                    row_limit: limit,
                    origin: req.origin,
                }))),
                _ => None,
            };
            if let Some(o) = &output {
                last_output = Some(o.handle.clone());
            }
            let (hist_status, rows, error) = match &outcome {
                Ok(o) => (
                    RunStatus::Success,
                    o.result
                        .as_ref()
                        .map(|r| r.total_rows as i64)
                        .or(o.rows_affected.map(|n| n as i64)),
                    None,
                ),
                Err(e) if e.kind == "cancelled" => (RunStatus::Cancelled, None, None),
                Err(e) => (RunStatus::Error, None, Some(e.message.clone())),
            };
            let _ = self.workspace.add_history(NewHistory {
                origin: req.origin,
                connection_id: Some(req.connection_id.clone()),
                sql: stmt.sql.clone(),
                started_at,
                duration_ms: duration_ms as i64,
                rows,
                status: Some(hist_status),
                error,
                output_handle: output.as_ref().map(|o| o.handle.clone()),
                result_id: output.as_ref().map(|o| o.result_id.clone()),
            });

            match outcome {
                Ok(o) => {
                    self.events.emit(JobEvent::StatementFinished {
                        job_id: job_id.clone(),
                        tab_id: req.tab_id.clone(),
                        index: stmt.index,
                        result: o.result,
                        output,
                        rows_affected: o.rows_affected,
                        duration_ms,
                        notices: o.notices,
                    });
                }
                Err(e) => {
                    status = if e.kind == "cancelled" {
                        RunStatus::Cancelled
                    } else {
                        RunStatus::Error
                    };
                    if matches!(e.kind.as_str(), "connection") {
                        self.drop_session(&req.connection_id, req.session_key());
                    }
                    self.events.emit(JobEvent::StatementFailed {
                        job_id: job_id.clone(),
                        tab_id: req.tab_id.clone(),
                        index: stmt.index,
                        error: e,
                        duration_ms,
                        statement_start: req.base_offset + stmt.start,
                    });
                    break;
                }
            }
        }

        if let (Some(name), Some(h)) = (req.output_name.as_deref().filter(|n| !n.is_empty()), &last_output) {
            let _ = self.outputs.set_name(h, Some(name));
        }
        self.jobs.lock().remove(&job_id);
        self.outputs_changed();
        self.events.emit(JobEvent::JobFinished {
            job_id,
            tab_id: req.tab_id,
            status,
            duration_ms: job_start.elapsed().as_millis() as u64,
        });
    }

    async fn run_statement(
        &self,
        job_id: &str,
        req: &RunRequest,
        stmt: &PlannedStatement,
        limit: Option<usize>,
        cancel: CancellationToken,
    ) -> Result<StatementOutcome> {
        let session = tokio::select! {
            s = self.session(&req.connection_id, req.session_key()) => s?,
            _ = cancel.cancelled() => return Err(ConnectorError::cancelled().into()),
        };
        let batch_size = limit.map(|l| l.clamp(1, 2000)).unwrap_or(2000);
        let mut stream = session
            .execute(
                &stmt.sql,
                ExecOptions {
                    batch_size,
                    cancel: cancel.clone(),
                    // One row past the cap tells us the result was truncated.
                    max_rows: limit.map(|l| l.saturating_add(1)),
                },
            )
            .await?;

        let result_id = format!("{job_id}:{}", stmt.index);
        let mut result = None;
        let mut notices = Vec::new();
        let mut truncated = false;
        let mut rows = 0usize;
        let t0 = Instant::now();
        let mut last_progress = Instant::now();

        loop {
            let Some(ev) = stream.next().await else {
                return Err(EngineError::new("internal", "result stream ended unexpectedly"));
            };
            match ev {
                Ok(StreamEvent::Schema(schema)) => {
                    result = Some(self.results.create(result_id.clone(), schema));
                }
                Ok(StreamEvent::Batch(mut batch)) => {
                    let Some(rs) = &result else { continue };
                    if let Some(l) = limit {
                        let room = l.saturating_sub(rows);
                        if batch.num_rows() > room {
                            // A row beyond the cap exists: keep `room` rows
                            // and stop the query.
                            truncated = true;
                            batch = batch.slice(0, room);
                        }
                    }
                    rows += batch.num_rows();
                    rs.lock().push(batch);
                    if truncated {
                        cancel.cancel();
                        break;
                    }
                    if last_progress.elapsed() >= PROGRESS_INTERVAL {
                        last_progress = Instant::now();
                        self.events.emit(JobEvent::Progress {
                            job_id: job_id.to_string(),
                            tab_id: req.tab_id.clone(),
                            index: stmt.index,
                            rows,
                            elapsed_ms: t0.elapsed().as_millis() as u64,
                        });
                    }
                }
                Ok(StreamEvent::Notice(n)) => notices.push(n),
                Ok(StreamEvent::Done(summary)) => {
                    let info = result.as_ref().map(|rs| {
                        let mut g = rs.lock();
                        g.finish(false);
                        g.info()
                    });
                    return Ok(StatementOutcome {
                        result: info,
                        rows_affected: summary.rows_affected,
                        notices,
                    });
                }
                Err(e) => {
                    self.results.remove(&result_id);
                    return Err(e.into());
                }
            }
        }

        // Only reached when stopped at the row cap.
        let info = result.as_ref().map(|rs| {
            let mut g = rs.lock();
            g.finish(true);
            g.info()
        });
        notices.push(format!(
            "Showing the first {} rows. Increase the row limit to fetch more.",
            rows
        ));
        // Wait for the connector to finish cancelling before the session is
        // reused; otherwise a server-side cancel could hit the next statement.
        let _ = tokio::time::timeout(Duration::from_secs(15), async {
            while stream.next().await.is_some() {}
        })
        .await;
        Ok(StatementOutcome {
            result: info,
            rows_affected: None,
            notices,
        })
    }
}

struct StatementOutcome {
    result: Option<ResultInfo>,
    rows_affected: Option<u64>,
    notices: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::{AuthMethod, MemoryStore};
    use databrain_connector_core::ConnectorKind;
    use databrain_connector_sqlite::SqliteConnector;
    use databrain_result_store::ViewSpec;

    struct Fixture {
        engine: Arc<QueryEngine>,
        sink: Arc<CollectingSink>,
        ws: Arc<Workspace>,
        conn_id: String,
        _dir: TempFile,
    }

    struct TempFile(std::path::PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn fixture(env: EnvTag) -> Fixture {
        let path = std::env::temp_dir().join(format!(
            "databrain-qe-{}-{}.db",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut reg = ConnectorRegistry::new();
        reg.register(Arc::new(SqliteConnector));
        let ws = Arc::new(Workspace::open_in_memory().unwrap());
        let profile = ws
            .save_connection(ConnectionProfile {
                id: String::new(),
                name: "Local".into(),
                config: {
                    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
                    c.file_path = Some(path.to_string_lossy().into());
                    c
                },
                color: None,
                env,
                folder_id: None,
                has_secret: false,
                ai_policy: Default::default(),
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        let sink = Arc::new(CollectingSink::default());
        let engine = QueryEngine::new(
            reg,
            ws.clone(),
            Arc::new(MemoryStore::default()),
            Arc::new(ResultStore::new()),
            sink.clone(),
        );
        Fixture {
            engine,
            sink,
            ws,
            conn_id: profile.id,
            _dir: TempFile(path),
        }
    }

    fn req(f: &Fixture, sql: &str) -> RunRequest {
        RunRequest {
            connection_id: f.conn_id.clone(),
            tab_id: "tab1".into(),
            sql: sql.into(),
            base_offset: 0,
            row_limit: Some(100),
            confirmed: false,
            origin: Origin::User,
            session_key: None,
            output_name: None,
        }
    }

    async fn wait_job(f: &Fixture, job_id: &str) -> Vec<JobEvent> {
        for _ in 0..500 {
            if !f.engine.is_running(job_id) {
                let evs = f.sink.0.lock().clone();
                if evs.iter().any(|e| matches!(e, JobEvent::JobFinished { job_id: j, .. } if j == job_id)) {
                    return evs
                        .into_iter()
                        .filter(|e| match e {
                            JobEvent::StatementStarted { job_id: j, .. }
                            | JobEvent::Progress { job_id: j, .. }
                            | JobEvent::StatementFinished { job_id: j, .. }
                            | JobEvent::StatementFailed { job_id: j, .. }
                            | JobEvent::JobFinished { job_id: j, .. }
                            | JobEvent::OutputsChanged { job_id: j, .. } => j == job_id,
                        })
                        .collect();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job did not finish");
    }

    fn started(r: RunResponse) -> String {
        match r {
            RunResponse::Started { job_id, .. } => job_id,
            other => panic!("expected start, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cells_share_session_but_keep_results() {
        let f = fixture(EnvTag::Dev);
        let cell = |tab: &str, sql: &str| RunRequest { tab_id: tab.into(), session_key: Some("nb:1".into()), ..req(&f, sql) };
        let j1 = started(f.engine.run(cell("nb:1:a", "create temp table tt as select 42 as v; select * from tt;")).unwrap());
        let e1 = wait_job(&f, &j1).await;
        let r1 = e1
            .iter()
            .find_map(|e| match e {
                JobEvent::StatementFinished { result: Some(r), .. } => Some(r.id.clone()),
                _ => None,
            })
            .unwrap();
        // Temp table (session state) is visible to another cell of the notebook.
        let j2 = started(f.engine.run(cell("nb:1:b", "select v from tt")).unwrap());
        let e2 = wait_job(&f, &j2).await;
        assert!(e2.iter().any(|e| matches!(e, JobEvent::StatementFinished { result: Some(r), .. } if r.total_rows == 1)), "{e2:?}");
        // Cell a's result survives cell b's run.
        assert!(f.engine.results().get(&r1).is_ok());
        // A different notebook/tab has its own session.
        let j3 = started(f.engine.run(req(&f, "select v from tt")).unwrap());
        let e3 = wait_job(&f, &j3).await;
        assert!(e3.iter().any(|e| matches!(e, JobEvent::StatementFailed { .. })));
        // Closing the notebook session releases it.
        f.engine.close_tab("nb:1");
        let j4 = started(f.engine.run(cell("nb:1:b", "select v from tt")).unwrap());
        assert!(wait_job(&f, &j4).await.iter().any(|e| matches!(e, JobEvent::StatementFailed { .. })));
    }

    #[tokio::test]
    async fn runs_script_and_records_history() {
        let f = fixture(EnvTag::Dev);
        let job = started(
            f.engine
                .run(req(
                    &f,
                    "create table t(id integer, name text);\ninsert into t values (1,'a'),(2,'b');\nselect * from t order by id;",
                ))
                .unwrap(),
        );
        let evs = wait_job(&f, &job).await;
        let finished: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                JobEvent::StatementFinished { result, rows_affected, .. } => {
                    Some((result.clone(), *rows_affected))
                }
                _ => None,
            })
            .collect();
        assert_eq!(finished.len(), 3);
        assert_eq!(finished[1].1, Some(2));
        let info = finished[2].0.clone().unwrap();
        assert_eq!(info.total_rows, 2);
        assert!(!info.truncated);
        let rs = f.engine.results().get(&info.id).unwrap();
        let page = rs.lock().page(&ViewSpec::default(), 0, 10).unwrap();
        assert_eq!(page.rows[1][1].as_deref(), Some("b"));
        assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Success, .. })));
        let hist = f.ws.list_history(&Default::default()).unwrap();
        assert_eq!(hist.len(), 3);
    }

    #[tokio::test]
    async fn row_limit_truncates() {
        let f = fixture(EnvTag::None);
        let mut r = req(
            &f,
            "with recursive c(x) as (select 1 union all select x+1 from c where x < 5000) select x from c",
        );
        r.row_limit = Some(250);
        let job = started(f.engine.run(r).unwrap());
        let evs = wait_job(&f, &job).await;
        let info = evs
            .iter()
            .find_map(|e| match e {
                JobEvent::StatementFinished { result, .. } => result.clone(),
                _ => None,
            })
            .unwrap();
        assert_eq!(info.total_rows, 250);
        assert!(info.truncated);

        // Exactly at the limit is not truncated.
        let mut r = req(&f, "select 1 union all select 2");
        r.row_limit = Some(2);
        let job = started(f.engine.run(r).unwrap());
        let evs = wait_job(&f, &job).await;
        let info = evs
            .iter()
            .find_map(|e| match e {
                JobEvent::StatementFinished { result, .. } => result.clone(),
                _ => None,
            })
            .unwrap();
        assert_eq!(info.total_rows, 2);
        assert!(!info.truncated);
    }

    #[tokio::test]
    async fn error_stops_script_with_offset() {
        let f = fixture(EnvTag::None);
        let mut r = req(&f, "select 1;\nselec 2;\nselect 3;");
        r.base_offset = 100;
        let job = started(f.engine.run(r).unwrap());
        let evs = wait_job(&f, &job).await;
        let failed = evs
            .iter()
            .find_map(|e| match e {
                JobEvent::StatementFailed { index, statement_start, error, .. } => {
                    Some((*index, *statement_start, error.kind.clone()))
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(failed, (1, 110, "query".to_string()));
        assert!(!evs.iter().any(|e| matches!(e, JobEvent::StatementStarted { index: 2, .. })));
        assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Error, .. })));
    }

    #[tokio::test]
    async fn cancel_long_query() {
        let f = fixture(EnvTag::None);
        let job = started(
            f.engine
                .run(req(
                    &f,
                    "with recursive c(x) as (select 1 union all select x+1 from c) select count(*) from c",
                ))
                .unwrap(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(f.engine.cancel(&job));
        let evs = wait_job(&f, &job).await;
        assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Cancelled, .. })));
        // Tab session still works afterwards.
        let job = started(f.engine.run(req(&f, "select 42")).unwrap());
        let evs = wait_job(&f, &job).await;
        assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Success, .. })));
    }

    #[tokio::test]
    async fn safety_confirmation() {
        let f = fixture(EnvTag::Prod);
        let r = f.engine.run(req(&f, "select 1")).unwrap();
        assert!(matches!(r, RunResponse::Started { .. }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        match f.engine.run(req(&f, "create table x(a int); delete from x")).unwrap() {
            RunResponse::NeedsConfirmation { reasons, .. } => {
                assert_eq!(reasons.len(), 2, "{reasons:?}");
            }
            other => panic!("{other:?}"),
        }
        let mut r = req(&f, "create table x(a int); delete from x");
        r.confirmed = true;
        let job = started(f.engine.run(r).unwrap());
        let evs = wait_job(&f, &job).await;
        assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Success, .. })));
    }

    #[tokio::test]
    async fn metadata_and_close_tab() {
        let f = fixture(EnvTag::None);
        let job = started(f.engine.run(req(&f, "create table users(id integer primary key); select * from users")).unwrap());
        wait_job(&f, &job).await;
        let schemas = f.engine.list_schemas(&f.conn_id).await.unwrap();
        assert_eq!(schemas[0].name, "main");
        let objs = f.engine.list_objects(&f.conn_id, "main").await.unwrap();
        assert_eq!(objs[0].name, "users");
        let d = f.engine.describe(&f.conn_id, "main", "users").await.unwrap();
        assert!(d.columns[0].is_primary_key);
        assert!(f.engine.connected_ids().contains(&f.conn_id));
        let before = f.engine.outputs().for_tab("tab1");
        assert_eq!(before[0].sql, "select * from users");
        f.engine.close_tab("tab1");
        assert!(f.engine.results().get(&before[0].result_id).is_err());
        f.engine.disconnect(&f.conn_id);
        assert!(f.engine.connected_ids().is_empty());
    }
}
