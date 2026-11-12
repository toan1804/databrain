//! Local persistence (SQLite): connection profiles, folders, saved queries,
//! query history, open tabs and settings. Secrets are never stored here;
//! connection profiles only reference keychain entries.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod ai;
pub mod ai_scope;
mod connections;
mod history;
pub mod jobs;
pub mod knowledge;
pub mod learned;
mod library;
pub mod meta_cache;
mod migrations;
pub mod notebooks;
pub mod outputs;
pub mod params;
mod tabs_settings;
#[cfg(test)]
mod tests;

pub use ai::{AiMessageRecord, AiProviderRecord, AiSessionRecord, AuditEntry};
pub use ai_scope::{AiCatalog, ScopeRule, scope_allows};
pub use jobs::{
    FileFormat, Job, JobEdge, JobNode, JobNodeKind, JobRun, JobSchedule, JobSummary, LoadMode, NodeRunRecord, NodeRunSummary, RunLogEntry, ScheduleMode,
    StepOwner, StepProgress,
};
pub use knowledge::{
    ImportAction, ImportItem, ImportKind, IndexDelta, KnHit, KnNote, KnObject, KnState, NoteEntry, NoteStatus, NotesFile, NotesSource, join_target,
    split_target, target_mentions,
};
pub use meta_cache::{CachedColumns, CachedListing, CachedTable, ExplorerCacheState};
pub use notebooks::{CellKind, CellRunSummary, Notebook, NotebookCell, NotebookSummary};
pub use outputs::OutputRecord;
pub use params::ParamSet;

pub use connections::{AiPolicy, ConnectionProfile, EnvTag, RunQueryPolicy};
pub use history::{
    AI_HISTORY_RETENTION_SETTING, HISTORY_LIMIT, HISTORY_RETENTION_DEFAULT_DAYS, HISTORY_RETENTION_SETTING, HistoryClear, HistoryEntry, HistoryQuery,
    NewHistory, RunStatus, history_terms,
};
pub use learned::{DocState, Documented, JoinDoc, JoinSighting, JoinStatus, LearnState, LearnedJoin, TableUsage};
pub use library::{Folder, FolderKind, Origin, SavedQuery};
pub use tabs_settings::TabState;

use migrations::{MIGRATIONS, check_schema_version};
use parking_lot::Mutex;
use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub struct Workspace {
    pub(crate) conn: Mutex<Connection>,
}

impl Workspace {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Invalid(format!("cannot create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let version = check_schema_version(&conn)?;
        // Before changing an existing workspace's schema, a copy as it was
        // (`workspace.db.bak-before-v<new>`), kept beside it.
        if version > 0 && (version as usize) < MIGRATIONS.len() {
            let backup = std::path::PathBuf::from(format!("{}.bak-before-v{}", path.as_ref().display(), MIGRATIONS.len()));
            if !backup.exists() {
                conn.execute_batch(&format!("VACUUM INTO '{}'", backup.to_string_lossy().replace('\'', "''")))
                    .map_err(|e| Error::Invalid(format!("cannot back up the workspace before updating it ({}): {e}", backup.display())))?;
            }
        }
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    /// The schema version this DataBrain knows (applied migrations).
    pub const SCHEMA_VERSION: usize = MIGRATIONS.len();

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Deleted or overwritten content (old SQL, connection settings) is
        // zeroed, not left in free pages of the file.
        conn.pragma_update(None, "secure_delete", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version = check_schema_version(&conn)?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(m)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        let ws = Self { conn: Mutex::new(conn) };
        // Best effort: a failure must not keep the workspace from opening.
        let _ = ws.redact_kept_sql();
        let _ = ws.redact_kept_transcripts();
        let _ = ws.mark_ai_written_jobs();
        ws.build_ai_tables()?;
        Ok(ws)
    }
}
