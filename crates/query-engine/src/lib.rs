//! Query orchestration: per-tab sessions, jobs that run scripts statement by
//! statement, row caps, cancellation, write-safety confirmation and history.
//!
//! UI-facing progress is reported through an [`EventSink`] so the engine has
//! no dependency on Tauri.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub mod compare;
mod connect;
pub mod edit;
mod error;
mod events;
mod export;
mod guard;
pub mod load;
mod metadata;
pub mod outputs;
pub mod plan;
pub mod probe;
pub mod related;
mod run;
pub mod schema_diff;
mod sessions;
pub mod testdata;

pub use error::*;
pub use events::*;
use guard::{Guarded, Hold, Intent};
pub use load::{LoadCheck, LoadDryRun, LoadEvent, LoadObserver, LoadReport, LoadRows, LoadSpec, LoadTarget, NoLoadObserver};
pub use outputs::{NewOutput, OutputInfo, OutputRegistry, OutputState, OutputTables};

use databrain_auth::{
    AuthStatus, CredentialSource, ExposeSecret, InlineCredentialSource, Interaction, NonInteractive, SecretRef, SecretStore, credential_source,
};
use databrain_connector_core::sql::{Classification, StatementKind, classify, split_statements};
use databrain_connector_core::{
    CancellationToken, ConnectionConfig, ConnectorError, ConnectorRegistry, DbObject, ErrorKind, ExecOptions, ObjectDetail, SchemaInfo, Session, StreamEvent,
    TableLayout,
};
use databrain_result_store::{ResultInfo, ResultStore};
use databrain_ssh_tunnel::{SshSecret, Tunnel, TunnelSpec};
use databrain_workspace::{ConnectionProfile, EnvTag, NewHistory, Origin, RunStatus, SessionRole, Workspace, now_ms};
use parking_lot::Mutex;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------ requests

#[derive(Debug, Clone, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
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
    #[ts(as = "Option<Origin>", optional)]
    pub origin: Origin,
    /// Share one database session between several run keys (notebook
    /// cells: each cell keeps its own results, all cells share temp tables
    /// and session settings). Defaults to `tab_id`.
    #[serde(default)]
    pub session_key: Option<String>,
    /// Name the job's last result (`results.<name>` in DuckDB sessions).
    #[serde(default)]
    pub output_name: Option<String>,
    /// Values of `:name` parameters in `sql` (editor parameter boxes).
    #[serde(default)]
    pub params: HashMap<String, databrain_connector_core::sql::ParamValue>,
    /// Output columns the statement must not see (the assistant's runs that
    /// read `results.<name>`); not sent by the UI.
    #[serde(skip)]
    #[ts(skip)]
    pub output_masks: Option<std::sync::Arc<databrain_connector_core::external::OutputMasks>>,
}

impl RunRequest {
    pub fn session_key(&self) -> &str {
        self.session_key.as_deref().filter(|k| !k.is_empty()).unwrap_or(&self.tab_id)
    }
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct PlannedStatement {
    pub index: usize,
    pub sql: String,
    pub start: usize,
    pub end: usize,
    pub classification: Classification,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
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

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct TestResult {
    pub server_version: String,
    #[ts(as = "f64")]
    pub latency_ms: u64,
}

// ------------------------------------------------------------------ engine

/// Run blocking work (file and workspace writes) from async code without
/// stalling the other tasks of a worker thread: on a multi-thread runtime
/// the thread is handed over while `f` runs; elsewhere `f` just runs.
/// Every runtime DataBrain runs on is multi-thread (the app's, Tauri's;
/// the MCP server's and the jobs runner's, `cli::runtime` in the app
/// crate); a current-thread one is only a test's (`#[tokio::test]`).
pub(crate) fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

struct JobHandle {
    /// The connection it runs on (`disconnect` stops it).
    connection_id: String,
    tab_id: String,
    session_key: String,
    cancel: CancellationToken,
    /// Its tab was closed while it ran: outputs it still adds are freed
    /// when it ends.
    tab_closed: bool,
}

/// Gets one run's outcome events (see [`QueryEngine::run_and_wait`]).
type Waiter = tokio::sync::mpsc::UnboundedSender<JobEvent>;

type SessionKey = (String, String); // (connection_id, tab_id)

/// The explorer's catalog session ([`SessionRole::Meta`]).
fn meta_tab() -> String {
    SessionRole::Meta.key()
}
/// Connection option marking the "Results (DuckDB)" connection.
pub const RESULTS_MARKER: &str = "databrain_results";

const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

/// A live session plus the SSH tunnel it runs through (kept alive together).
struct Live {
    session: Arc<dyn Session>,
    _tunnel: Option<Arc<Tunnel>>,
    /// Opened for SQL from the AI or MCP in a sandbox (DuckDB).
    sandboxed: bool,
    /// Last time a statement or catalog call used it.
    last_used: parking_lot::Mutex<Instant>,
}

/// Setting: close sessions unused for this many minutes (0 = never).
pub const IDLE_DISCONNECT_SETTING: &str = "idle_disconnect_minutes";
/// Default for [`IDLE_DISCONNECT_SETTING`].
pub const IDLE_DISCONNECT_DEFAULT_MINUTES: u64 = 60;

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
    /// Running connection tests (Test button), cancellable by id.
    tests: Mutex<HashMap<String, CancellationToken>>,
    /// Running full exports (query re-run straight to a file), by id.
    exports: Mutex<HashMap<String, CancellationToken>>,
    /// Files written by finished full exports, by id.
    /// Files of the latest exports (export id, path), newest last, at most
    /// [`export::KEPT_EXPORTS`].
    exported: Mutex<std::collections::VecDeque<(String, std::path::PathBuf)>>,
    outputs: Arc<OutputRegistry>,
    /// Session keys (tabs) in manual-commit mode: writes open a transaction
    /// the user ends with Commit or Rollback.
    manual_commit: Mutex<std::collections::HashSet<String>>,
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
            tests: Mutex::new(HashMap::new()),
            exports: Mutex::new(HashMap::new()),
            exported: Mutex::new(std::collections::VecDeque::new()),
            outputs: OutputRegistry::new(results.clone(), workspace.clone()),
            manual_commit: Mutex::new(Default::default()),
            results,
        })
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
}

struct StatementOutcome {
    result: Option<ResultInfo>,
    rows_affected: Option<u64>,
    notices: Vec<String>,
    server_ms: Option<u64>,
}

#[cfg(test)]
mod tests;
