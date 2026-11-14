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
pub mod paths;
mod session_role;
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
pub use paths::{path_parts, quote_name, split_reference, table_path};
pub use session_role::{AiPart, SessionRole};

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
        // Another process may be opening it too (the app and databrain-mcp).
        conn.busy_timeout(std::time::Duration::from_secs(120))?;
        // WAL (kept in the file once set). Switching needs a moment no other
        // connection reads, and SQLite doesn't wait for it: try again.
        let mut tries = 0;
        while let Err(e) = conn.pragma_update(None, "journal_mode", "WAL") {
            let busy =
                matches!(&e, rusqlite::Error::SqliteFailure(f, _) if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked));
            tries += 1;
            if !busy || tries > 200 {
                return Err(e.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let version = check_schema_version(&conn)?;
        // Before changing an existing workspace's schema, a copy as it was
        // (`workspace.db.bak-before-v<new>`), kept beside it.
        if version > 0 && (version as usize) < MIGRATIONS.len() {
            let backup = std::path::PathBuf::from(format!("{}.bak-before-v{}", path.as_ref().display(), MIGRATIONS.len()));
            if !backup.exists() {
                // Written beside it, then moved into place: another process
                // backing up at the same moment doesn't write the same file.
                static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let tmp = std::path::PathBuf::from(format!("{}.tmp-{}-{n}", backup.display(), std::process::id()));
                let fail = |e: String| Error::Invalid(format!("cannot back up the workspace before updating it ({}): {e}", backup.display()));
                conn.execute_batch(&format!("VACUUM INTO '{}'", tmp.to_string_lossy().replace('\'', "''"))).map_err(|e| fail(e.to_string()))?;
                if backup.exists() {
                    let _ = std::fs::remove_file(&tmp);
                } else {
                    std::fs::rename(&tmp, &backup).map_err(|e| fail(e.to_string()))?;
                }
            }
        }
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    /// The schema version this DataBrain knows (applied migrations).
    pub const SCHEMA_VERSION: usize = MIGRATIONS.len();

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Deleted or overwritten content (old SQL, connection settings) is
        // zeroed, not left in free pages of the file.
        conn.pragma_update(None, "secure_delete", "ON")?;
        // Migrations: each step takes the write lock first and reads the
        // version under it, so two processes upgrading at once (the app and
        // databrain-mcp) apply each step once: the second waits, then finds
        // it done. A long migration may keep it waiting: a longer timeout.
        conn.busy_timeout(std::time::Duration::from_secs(120))?;
        loop {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let version = check_schema_version(&tx)?;
            let Some(m) = MIGRATIONS.get(version as usize) else { break };
            tx.execute_batch(m)?;
            tx.pragma_update(None, "user_version", version + 1)?;
            tx.commit()?;
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let ws = Self { conn: Mutex::new(conn) };
        // Best effort: a failure must not keep the workspace from opening.
        let _ = ws.redact_kept_sql();
        let _ = ws.redact_kept_transcripts();
        let _ = ws.mark_ai_written_jobs();
        // Also best effort, and failing closed: without it the AI's table
        // list stays empty (it sees no table) and it is built again on the
        // next start (its marker is written with it).
        if let Err(e) = ws.build_ai_tables() {
            tracing::warn!("building the AI's table list failed; retried on the next start: {e}");
        }
        Ok(ws)
    }
}
