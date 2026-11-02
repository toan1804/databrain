//! Events of runs and exports, and where they go (the UI, and in-process
//! subscribers such as the AI agent).

use super::*;

// ------------------------------------------------------------------ events

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
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
        #[ts(as = "f64")]
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
        #[ts(as = "Option<f64>")]
        rows_affected: Option<u64>,
        #[ts(as = "f64")]
        duration_ms: u64,
        notices: Vec<String>,
        /// Time the server took to run it, when the connector reports it
        /// separately from the download (see [`JobEvent::Downloading`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(as = "Option<f64>", optional)]
        server_ms: Option<u64>,
    },
    /// The server finished running the statement and the result is being
    /// downloaded (`elapsed_ms` = time on the server). `notice`: shown to
    /// the user (large results fetched through external links).
    Downloading {
        job_id: String,
        tab_id: String,
        index: usize,
        notice: Option<String>,
        #[ts(as = "f64")]
        elapsed_ms: u64,
    },
    StatementFailed {
        job_id: String,
        tab_id: String,
        index: usize,
        error: EngineError,
        #[ts(as = "f64")]
        duration_ms: u64,
        /// Byte offset of the statement within the submitted text, so the
        /// editor can map `error.position` to a location.
        statement_start: usize,
    },
    JobFinished {
        job_id: String,
        tab_id: String,
        status: RunStatus,
        #[ts(as = "f64")]
        duration_ms: u64,
        /// The run's session has a transaction open afterwards.
        #[serde(default)]
        in_transaction: bool,
    },
    /// Outputs were added, renamed, pinned or evicted (job/tab ids empty).
    OutputsChanged {
        job_id: String,
        tab_id: String,
    },
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: JobEvent);
}

/// Stage of a full export ([`QueryEngine::export_query`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportPhase {
    /// Opening a session of its own.
    Connecting,
    /// The query runs; no rows yet.
    Running,
    /// The server finished; the result is downloaded from cloud storage.
    Downloading,
    /// Rows are being written to the file.
    Writing,
    /// All rows are in; the file is being completed (Parquet/XLSX footer).
    Finishing,
}

/// Progress of a full export: rows written so far and the file's size.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ExportProgress {
    pub phase: ExportPhase,
    pub rows: u64,
    pub bytes: u64,
}

/// Minimum time between two "rows written" reports of a full export.
pub const EXPORT_PROGRESS_EVERY: Duration = Duration::from_millis(250);

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
            | JobEvent::Downloading { job_id, .. }
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
