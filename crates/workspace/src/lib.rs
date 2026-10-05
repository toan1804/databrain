//! Local persistence (SQLite): connection profiles, folders, saved queries,
//! query history, open tabs and settings. Secrets are never stored here;
//! connection profiles only reference keychain entries.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod ai;
pub mod knowledge;
pub mod meta_cache;
pub mod notebooks;
pub mod outputs;

pub use ai::{AiMessageRecord, AiProviderRecord, AiSessionRecord, AuditEntry};
pub use outputs::OutputRecord;
pub use notebooks::{CellKind, CellRunSummary, Notebook, NotebookCell, NotebookSummary};
pub use knowledge::{join_target, split_target, target_mentions, ImportAction, ImportItem, ImportKind, IndexDelta, KnHit, KnNote, KnObject, KnState, NoteEntry, NoteStatus, NotesFile, NotesSource};

use databrain_connector_core::ConnectionConfig;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvTag {
    #[default]
    None,
    Dev,
    Staging,
    Prod,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub config: ConnectionConfig,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub env: EnvTag,
    #[serde(default)]
    pub folder_id: Option<String>,
    /// Whether a secret was saved in the keychain for this connection.
    #[serde(default)]
    pub has_secret: bool,
    #[serde(default)]
    pub ai_policy: AiPolicy,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunQueryPolicy {
    /// The AI may not execute queries.
    Never,
    /// Every AI-proposed query needs approval.
    #[default]
    Ask,
    /// Read-only statements run without asking; writes still ask.
    AutoRead,
}

/// Per-connection controls on what the AI may see and do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiPolicy {
    pub ai_enabled: bool,
    /// Provider ids allowed for this connection (empty = any).
    pub allowed_providers: Vec<String>,
    /// Schema/table/column names and comments.
    pub share_metadata: bool,
    /// A few distinct values per column when describing tables.
    pub share_sample_values: bool,
    /// Raw result rows (otherwise only aggregates computed locally).
    pub share_result_rows: bool,
    pub run_query: RunQueryPolicy,
    /// Allow AI-proposed INSERT/UPDATE/DELETE/DDL (always with approval).
    pub allow_write: bool,
    pub max_rows_to_model: u32,
    /// `table.column` or `column` names masked in anything sent to the model.
    pub pii_columns: Vec<String>,
    /// Schemas to include in the knowledge index (empty = all).
    pub index_schemas: Vec<String>,
    /// Schemas fetched per metadata request while indexing (bigger = fewer
    /// queries on engines with bulk metadata, e.g. Databricks).
    pub index_batch: u32,
    /// Expose this connection to external agents through the MCP server.
    pub mcp_enabled: bool,
    /// Notes the AI adds or updates are saved directly instead of waiting
    /// for review in Knowledge.
    pub auto_approve_notes: bool,
}

impl Default for AiPolicy {
    fn default() -> Self {
        Self {
            ai_enabled: true,
            allowed_providers: vec![],
            share_metadata: true,
            share_sample_values: false,
            share_result_rows: false,
            run_query: RunQueryPolicy::Ask,
            allow_write: false,
            max_rows_to_model: 50,
            pii_columns: vec![],
            index_schemas: vec![],
            index_batch: 25,
            mcp_enabled: false,
            auto_approve_notes: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    #[default]
    User,
    Ai,
    Mcp,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::User => "user",
            Origin::Ai => "ai",
            Origin::Mcp => "mcp",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "ai" => Origin::Ai,
            "mcp" => Origin::Mcp,
            _ => Origin::User,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderKind {
    Connections,
    Queries,
    Notebooks,
}

impl FolderKind {
    fn as_str(self) -> &'static str {
        match self {
            FolderKind::Connections => "connections",
            FolderKind::Queries => "queries",
            FolderKind::Notebooks => "notebooks",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "connections" => FolderKind::Connections,
            "notebooks" => FolderKind::Notebooks,
            _ => FolderKind::Queries,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub name: String,
    pub kind: FolderKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedQuery {
    pub id: String,
    pub name: String,
    pub sql: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Offered to the AI as a few-shot example for its connection.
    #[serde(default)]
    pub ai_example: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Success,
    Error,
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Success => "success",
            RunStatus::Error => "error",
            RunStatus::Cancelled => "cancelled",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "error" => RunStatus::Error,
            "cancelled" => RunStatus::Cancelled,
            _ => RunStatus::Success,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    #[serde(default)]
    pub origin: Origin,
    pub connection_id: Option<String>,
    pub connection_name: Option<String>,
    pub sql: String,
    pub started_at: i64,
    pub duration_ms: i64,
    pub rows: Option<i64>,
    pub status: RunStatus,
    pub error: Option<String>,
    /// Output produced by this statement (`r12`) and its result id.
    #[serde(default)]
    pub output_handle: Option<String>,
    #[serde(default)]
    pub result_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NewHistory {
    pub origin: Origin,
    pub connection_id: Option<String>,
    pub sql: String,
    pub started_at: i64,
    pub duration_ms: i64,
    pub rows: Option<i64>,
    pub status: Option<RunStatus>,
    pub error: Option<String>,
    pub output_handle: Option<String>,
    pub result_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Return entries with `id < before` (pagination).
    #[serde(default)]
    pub before: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabState {
    pub id: String,
    pub title: String,
    pub sql: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub saved_query_id: Option<String>,
    /// Set for notebook tabs.
    #[serde(default)]
    pub notebook_id: Option<String>,
    /// Set for output viewer tabs (output handle).
    #[serde(default)]
    pub output_ref: Option<String>,
}

const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE folders (
        id TEXT PRIMARY KEY,
        parent_id TEXT REFERENCES folders(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        kind TEXT NOT NULL
    );
    CREATE TABLE connections (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        kind TEXT NOT NULL,
        config_json TEXT NOT NULL,
        color TEXT,
        env TEXT NOT NULL DEFAULT 'none',
        folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
        has_secret INTEGER NOT NULL DEFAULT 0,
        sort_order INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE saved_queries (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        sql TEXT NOT NULL,
        connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
        folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
        description TEXT,
        tags_json TEXT NOT NULL DEFAULT '[]',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE query_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        connection_id TEXT,
        sql TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        duration_ms INTEGER NOT NULL,
        rows INTEGER,
        status TEXT NOT NULL,
        error TEXT
    );
    CREATE INDEX query_history_started ON query_history(started_at DESC);
    CREATE TABLE tabs (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        sql TEXT NOT NULL,
        connection_id TEXT,
        saved_query_id TEXT,
        order_idx INTEGER NOT NULL
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value_json TEXT NOT NULL
    );
    "#,
    // v2: AI mode + knowledge
    r#"
    ALTER TABLE connections ADD COLUMN ai_policy_json TEXT;
    ALTER TABLE saved_queries ADD COLUMN ai_example INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE query_history ADD COLUMN origin TEXT NOT NULL DEFAULT 'user';
    CREATE TABLE ai_providers (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        config_json TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE ai_sessions (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        connection_id TEXT,
        provider_id TEXT,
        model TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE ai_messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL REFERENCES ai_sessions(id) ON DELETE CASCADE,
        role TEXT NOT NULL,
        content_json TEXT NOT NULL,
        tokens_in INTEGER,
        tokens_out INTEGER,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX ai_messages_session ON ai_messages(session_id, id);
    CREATE TABLE ai_audit (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT,
        connection_id TEXT,
        tool TEXT NOT NULL,
        args_json TEXT NOT NULL,
        decision TEXT NOT NULL,
        summary TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE kn_objects (
        connection_id TEXT NOT NULL,
        schema_name TEXT NOT NULL,
        name TEXT NOT NULL,
        kind TEXT NOT NULL,
        comment TEXT,
        row_estimate INTEGER,
        columns_json TEXT NOT NULL,
        fks_json TEXT NOT NULL,
        content_hash TEXT NOT NULL,
        indexed_at INTEGER NOT NULL,
        PRIMARY KEY (connection_id, schema_name, name)
    );
    CREATE TABLE kn_notes (
        id TEXT PRIMARY KEY,
        connection_id TEXT NOT NULL,
        target TEXT,
        body TEXT NOT NULL,
        author TEXT NOT NULL,
        status TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE kn_state (
        connection_id TEXT PRIMARY KEY,
        indexed_at INTEGER NOT NULL,
        objects INTEGER NOT NULL,
        schemas_json TEXT NOT NULL,
        error TEXT
    );
    CREATE VIRTUAL TABLE kn_fts USING fts5(
        connection_id UNINDEXED, source UNINDEXED, ref UNINDEXED, title, body,
        tokenize = "unicode61 remove_diacritics 2 tokenchars '_'"
    );
    "#,
    // v3: notebooks
    r#"
    CREATE TABLE notebooks (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
        folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
        cells_json TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    ALTER TABLE tabs ADD COLUMN notebook_id TEXT;
    "#,
    // v4: query outputs (handles, pins, snapshots) + links
    r#"
    CREATE TABLE outputs (
        result_id     TEXT PRIMARY KEY,
        handle        TEXT NOT NULL UNIQUE,
        name          TEXT,
        connection_id TEXT,
        meta_json     TEXT NOT NULL,
        snapshot_path TEXT,
        created_at    INTEGER NOT NULL
    );
    ALTER TABLE query_history ADD COLUMN output_handle TEXT;
    ALTER TABLE query_history ADD COLUMN result_id TEXT;
    ALTER TABLE tabs ADD COLUMN output_ref TEXT;
    "#,
    // v5: AI note updates point at the note they replace
    r#"
    ALTER TABLE kn_notes ADD COLUMN replaces TEXT;
    "#,
    // v6: metadata cache (tables and columns seen while browsing, for completion)
    r#"
    CREATE TABLE meta_objects (
        connection_id TEXT NOT NULL,
        schema_name   TEXT NOT NULL,
        name          TEXT NOT NULL,
        kind          TEXT NOT NULL,
        comment       TEXT,
        row_estimate  INTEGER,
        seen_at       INTEGER NOT NULL,
        PRIMARY KEY (connection_id, schema_name, name)
    );
    CREATE TABLE meta_columns (
        connection_id TEXT NOT NULL,
        schema_name   TEXT NOT NULL,
        table_name    TEXT NOT NULL,
        columns_json  TEXT NOT NULL,
        seen_at       INTEGER NOT NULL,
        PRIMARY KEY (connection_id, schema_name, table_name)
    );
    "#,
    // v7: per-schema fingerprints for incremental AI indexing
    r#"
    CREATE TABLE kn_schema_state (
        connection_id TEXT NOT NULL,
        schema_name   TEXT NOT NULL,
        fingerprint   TEXT NOT NULL,
        indexed_at    INTEGER NOT NULL,
        PRIMARY KEY (connection_id, schema_name)
    );
    "#,
];

/// Maximum history rows kept; older rows are pruned on insert.
pub const HISTORY_LIMIT: i64 = 5000;

pub struct Workspace {
    pub(crate) conn: Mutex<Connection>,
}

impl Workspace {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Invalid(format!("cannot create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(m)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ---------------------------------------------------------------- connections

    pub fn list_connections(&self) -> Result<Vec<ConnectionProfile>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, name, config_json, color, env, folder_id, has_secret, created_at, updated_at, ai_policy_json \
             FROM connections ORDER BY sort_order, name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], connection_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().collect()
    }

    pub fn get_connection(&self, id: &str) -> Result<ConnectionProfile> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT id, name, config_json, color, env, folder_id, has_secret, created_at, updated_at, ai_policy_json \
             FROM connections WHERE id = ?1",
            [id],
            connection_row,
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("connection {id}")))?
    }

    /// Insert or update. Empty `id` creates a new profile. Returns the saved profile.
    pub fn save_connection(&self, mut p: ConnectionProfile) -> Result<ConnectionProfile> {
        if p.name.trim().is_empty() {
            return Err(Error::Invalid("connection name is required".into()));
        }
        let now = now_ms();
        if p.id.is_empty() {
            p.id = new_id();
            p.created_at = now;
        }
        p.updated_at = now;
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO connections (id, name, kind, config_json, color, env, folder_id, has_secret, created_at, updated_at, ai_policy_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, kind = excluded.kind, \
               config_json = excluded.config_json, color = excluded.color, env = excluded.env, \
               folder_id = excluded.folder_id, has_secret = excluded.has_secret, updated_at = excluded.updated_at, \
               ai_policy_json = excluded.ai_policy_json",
            params![
                p.id,
                p.name.trim(),
                p.config.kind.as_str(),
                serde_json::to_string(&p.config)?,
                p.color,
                serde_json::to_value(p.env)?.as_str().unwrap_or("none"),
                p.folder_id,
                p.has_secret,
                if p.created_at == 0 { now } else { p.created_at },
                p.updated_at,
                serde_json::to_string(&p.ai_policy)?,
            ],
        )?;
        drop(c);
        self.get_connection(&p.id)
    }

    /// Delete a connection and the data that only belongs to it (AI
    /// knowledge: indexed metadata, notes, index state). Saved queries,
    /// history, notebooks and AI conversations are kept (their connection
    /// becomes unset). Secrets are removed by the caller (secret store).
    pub fn delete_connection(&self, id: &str) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let n = tx.execute("DELETE FROM connections WHERE id = ?1", [id])?;
        if n == 0 {
            return Err(Error::NotFound(format!("connection {id}")));
        }
        tx.execute("DELETE FROM kn_objects WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM kn_fts WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM kn_notes WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM kn_state WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM meta_objects WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM kn_schema_state WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM meta_columns WHERE connection_id = ?1", [id])?;
        tx.execute("UPDATE tabs SET connection_id = NULL WHERE connection_id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    // ---------------------------------------------------------------- folders

    pub fn list_folders(&self, kind: FolderKind) -> Result<Vec<Folder>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, parent_id, name, kind FROM folders WHERE kind = ?1 ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([kind.as_str()], |r| {
                Ok(Folder {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    name: r.get(2)?,
                    kind: FolderKind::parse(&r.get::<_, String>(3)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn save_folder(&self, mut f: Folder) -> Result<Folder> {
        if f.name.trim().is_empty() {
            return Err(Error::Invalid("folder name is required".into()));
        }
        if f.id.is_empty() {
            f.id = new_id();
        }
        if f.parent_id.as_deref() == Some(f.id.as_str()) {
            return Err(Error::Invalid("a folder cannot be its own parent".into()));
        }
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO folders (id, parent_id, name, kind) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(id) DO UPDATE SET parent_id = excluded.parent_id, name = excluded.name",
            params![f.id, f.parent_id, f.name.trim(), f.kind.as_str()],
        )?;
        f.name = f.name.trim().to_string();
        Ok(f)
    }

    /// Move a connection, saved query or notebook into a folder (`None` = root).
    pub fn move_to_folder(&self, kind: FolderKind, item_id: &str, folder_id: Option<&str>) -> Result<()> {
        let c = self.conn.lock();
        let table = match kind {
            FolderKind::Connections => "connections",
            FolderKind::Queries => "saved_queries",
            FolderKind::Notebooks => "notebooks",
        };
        let n = c.execute(&format!("UPDATE {table} SET folder_id = ?1 WHERE id = ?2"), params![folder_id, item_id])?;
        if n == 0 {
            return Err(Error::NotFound(item_id.to_string()));
        }
        Ok(())
    }

    /// Deletes the folder and its sub-folders; contained items move to the root.
    pub fn delete_folder(&self, id: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM folders WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---------------------------------------------------------------- saved queries

    pub fn list_saved_queries(&self, search: Option<&str>) -> Result<Vec<SavedQuery>> {
        let c = self.conn.lock();
        let base = "SELECT id, name, sql, connection_id, folder_id, description, tags_json, created_at, updated_at, ai_example FROM saved_queries";
        let rows = match search.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => {
                let pat = format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
                let mut stmt = c.prepare(&format!(
                    "{base} WHERE name LIKE ?1 ESCAPE '\\' OR sql LIKE ?1 ESCAPE '\\' \
                     OR description LIKE ?1 ESCAPE '\\' OR tags_json LIKE ?1 ESCAPE '\\' \
                     ORDER BY updated_at DESC"
                ))?;
                stmt.query_map([pat], saved_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = c.prepare(&format!("{base} ORDER BY name COLLATE NOCASE"))?;
                stmt.query_map([], saved_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    pub fn get_saved_query(&self, id: &str) -> Result<SavedQuery> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT id, name, sql, connection_id, folder_id, description, tags_json, created_at, updated_at, ai_example \
             FROM saved_queries WHERE id = ?1",
            [id],
            saved_row,
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("saved query {id}")))
    }

    pub fn save_query(&self, mut q: SavedQuery) -> Result<SavedQuery> {
        if q.name.trim().is_empty() {
            return Err(Error::Invalid("query name is required".into()));
        }
        let now = now_ms();
        if q.id.is_empty() {
            q.id = new_id();
            q.created_at = now;
        }
        q.updated_at = now;
        q.name = q.name.trim().to_string();
        q.tags = q
            .tags
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO saved_queries (id, name, sql, connection_id, folder_id, description, tags_json, created_at, updated_at, ai_example) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, sql = excluded.sql, \
               connection_id = excluded.connection_id, folder_id = excluded.folder_id, \
               description = excluded.description, tags_json = excluded.tags_json, updated_at = excluded.updated_at, \
               ai_example = excluded.ai_example",
            params![
                q.id,
                q.name,
                q.sql,
                q.connection_id,
                q.folder_id,
                q.description,
                serde_json::to_string(&q.tags)?,
                if q.created_at == 0 { now } else { q.created_at },
                q.updated_at,
                q.ai_example,
            ],
        )?;
        drop(c);
        self.get_saved_query(&q.id)
    }

    pub fn delete_saved_query(&self, id: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM saved_queries WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---------------------------------------------------------------- history

    pub fn add_history(&self, h: NewHistory) -> Result<i64> {
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO query_history (connection_id, sql, started_at, duration_ms, rows, status, error, origin, output_handle, result_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                h.connection_id,
                h.sql,
                h.started_at,
                h.duration_ms,
                h.rows,
                h.status.unwrap_or(RunStatus::Success).as_str(),
                h.error,
                h.origin.as_str(),
                h.output_handle,
                h.result_id,
            ],
        )?;
        let id = c.last_insert_rowid();
        c.execute(
            "DELETE FROM query_history WHERE id <= ?1 - ?2",
            params![id, HISTORY_LIMIT],
        )?;
        Ok(id)
    }

    pub fn list_history(&self, q: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        let c = self.conn.lock();
        let limit = q.limit.unwrap_or(200).min(1000) as i64;
        let search = q
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let mut stmt = c.prepare(
            "SELECT h.id, h.connection_id, c.name, h.sql, h.started_at, h.duration_ms, h.rows, h.status, h.error, h.origin, h.output_handle, h.result_id \
             FROM query_history h LEFT JOIN connections c ON c.id = h.connection_id \
             WHERE (?1 IS NULL OR h.sql LIKE ?1 ESCAPE '\\') \
               AND (?2 IS NULL OR h.connection_id = ?2) \
               AND (?3 IS NULL OR h.id < ?3) \
             ORDER BY h.id DESC LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(params![search, q.connection_id, q.before, limit], |r| {
                Ok(HistoryEntry {
                    origin: Origin::parse(&r.get::<_, String>(9)?),
                    id: r.get(0)?,
                    connection_id: r.get(1)?,
                    connection_name: r.get(2)?,
                    sql: r.get(3)?,
                    started_at: r.get(4)?,
                    duration_ms: r.get(5)?,
                    rows: r.get(6)?,
                    status: RunStatus::parse(&r.get::<_, String>(7)?),
                    error: r.get(8)?,
                    output_handle: r.get(10)?,
                    result_id: r.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn clear_history(&self) -> Result<()> {
        self.conn.lock().execute("DELETE FROM query_history", [])?;
        Ok(())
    }

    // ---------------------------------------------------------------- tabs

    pub fn list_tabs(&self) -> Result<Vec<TabState>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, title, sql, connection_id, saved_query_id, notebook_id, output_ref FROM tabs ORDER BY order_idx",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TabState {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    sql: r.get(2)?,
                    connection_id: r.get(3)?,
                    saved_query_id: r.get(4)?,
                    notebook_id: r.get(5)?,
                    output_ref: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Replace the stored tab set (called on change, debounced by the UI).
    pub fn save_tabs(&self, tabs: &[TabState]) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM tabs", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO tabs (id, title, sql, connection_id, saved_query_id, order_idx, notebook_id, output_ref) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (i, t) in tabs.iter().enumerate() {
                stmt.execute(params![t.id, t.title, t.sql, t.connection_id, t.saved_query_id, i as i64, t.notebook_id, t.output_ref])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---------------------------------------------------------------- settings

    pub fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let c = self.conn.lock();
        let v: Option<String> = c
            .query_row("SELECT value_json FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()?;
        Ok(match v {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub fn all_settings(&self) -> Result<serde_json::Map<String, serde_json::Value>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare("SELECT key, value_json FROM settings")?;
        let mut out = serde_json::Map::new();
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (k, v) = row?;
            out.insert(k, serde_json::from_str(&v)?);
        }
        Ok(out)
    }

    pub fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO settings (key, value_json) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }
}

fn connection_row(r: &Row<'_>) -> rusqlite::Result<Result<ConnectionProfile>> {
    let config_json: String = r.get(2)?;
    let env: String = r.get(4)?;
    let id: String = r.get(0)?;
    let name: String = r.get(1)?;
    let color: Option<String> = r.get(3)?;
    let folder_id: Option<String> = r.get(5)?;
    let has_secret: bool = r.get(6)?;
    let created_at: i64 = r.get(7)?;
    let updated_at: i64 = r.get(8)?;
    let policy: Option<String> = r.get(9)?;
    Ok((|| {
        Ok(ConnectionProfile {
            ai_policy: policy.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default(),
            id,
            name,
            config: serde_json::from_str(&config_json)?,
            color,
            env: serde_json::from_value(serde_json::Value::String(env)).unwrap_or_default(),
            folder_id,
            has_secret,
            created_at,
            updated_at,
        })
    })())
}

fn saved_row(r: &Row<'_>) -> rusqlite::Result<SavedQuery> {
    let tags: String = r.get(6)?;
    Ok(SavedQuery {
        id: r.get(0)?,
        name: r.get(1)?,
        sql: r.get(2)?,
        connection_id: r.get(3)?,
        folder_id: r.get(4)?,
        description: r.get(5)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        ai_example: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectorKind;

    fn profile(name: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: String::new(),
            name: name.into(),
            config: {
                let mut c = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: "me".into() });
                c.host = Some("db.local".into());
                c.port = Some(5432);
                c.database = Some("app".into());
                c
            },
            color: Some("#ef4444".into()),
            env: EnvTag::Prod,
            folder_id: None,
            has_secret: true,
            ai_policy: AiPolicy::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn connections_crud() {
        let ws = Workspace::open_in_memory().unwrap();
        let saved = ws.save_connection(profile("Prod DB")).unwrap();
        assert!(!saved.id.is_empty());
        assert_eq!(saved.env, EnvTag::Prod);
        assert_eq!(saved.ai_policy, AiPolicy::default());
        assert_eq!(saved.config.host.as_deref(), Some("db.local"));
        let mut edited = saved.clone();
        edited.name = "Production".into();
        ws.save_connection(edited).unwrap();
        let all = ws.list_connections().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Production");
        assert_eq!(all[0].created_at, saved.created_at);
        ws.delete_connection(&saved.id).unwrap();
        assert!(ws.list_connections().unwrap().is_empty());
        assert!(ws.save_connection(profile("  ")).is_err());
    }

    #[test]
    fn saved_queries_and_folders() {
        let ws = Workspace::open_in_memory().unwrap();
        let f = ws
            .save_folder(Folder {
                id: String::new(),
                parent_id: None,
                name: "Reports".into(),
                kind: FolderKind::Queries,
            })
            .unwrap();
        let q = ws
            .save_query(SavedQuery {
                id: String::new(),
                name: "Monthly revenue".into(),
                sql: "select sum(amount) from orders".into(),
                connection_id: None,
                folder_id: Some(f.id.clone()),
                description: None,
                tags: vec!["finance".into(), " ".into()],
                ai_example: true,
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        assert_eq!(q.tags, vec!["finance"]);
        assert_eq!(ws.list_saved_queries(Some("orders")).unwrap().len(), 1);
        assert_eq!(ws.list_saved_queries(Some("finance")).unwrap().len(), 1);
        assert_eq!(ws.list_saved_queries(Some("100%")).unwrap().len(), 0);
        assert!(ws.get_saved_query(&q.id).unwrap().ai_example);
        ws.move_to_folder(FolderKind::Queries, &q.id, None).unwrap();
        assert_eq!(ws.get_saved_query(&q.id).unwrap().folder_id, None);
        ws.move_to_folder(FolderKind::Queries, &q.id, Some(&f.id)).unwrap();
        ws.delete_folder(&f.id).unwrap();
        assert_eq!(ws.get_saved_query(&q.id).unwrap().folder_id, None);
        ws.delete_saved_query(&q.id).unwrap();
        assert!(ws.list_saved_queries(None).unwrap().is_empty());
    }

    #[test]
    fn history_tabs_settings() {
        let ws = Workspace::open_in_memory().unwrap();
        let conn = ws.save_connection(profile("A")).unwrap();
        for i in 0..3 {
            ws.add_history(NewHistory {
                origin: if i == 1 { Origin::Ai } else { Origin::User },
                connection_id: Some(conn.id.clone()),
                sql: format!("select {i}"),
                started_at: now_ms(),
                duration_ms: 5,
                rows: Some(1),
                status: Some(if i == 2 { RunStatus::Error } else { RunStatus::Success }),
                error: None,
                output_handle: (i == 0).then(|| "r1".to_string()),
                result_id: (i == 0).then(|| "job:0".to_string()),
            })
            .unwrap();
        }
        let h = ws.list_history(&HistoryQuery::default()).unwrap();
        assert_eq!(h.last().unwrap().output_handle.as_deref(), Some("r1"));
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].sql, "select 2");
        assert_eq!(h[0].status, RunStatus::Error);
        assert_eq!(h[0].connection_name.as_deref(), Some("A"));
        assert_eq!(h[1].origin, Origin::Ai);
        let h = ws
            .list_history(&HistoryQuery {
                search: Some("1".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(h.len(), 1);
        let page = ws
            .list_history(&HistoryQuery {
                before: Some(h[0].id),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.len(), 1);

        let tabs = vec![
            TabState {
                id: "t1".into(),
                title: "Query 1".into(),
                sql: "select 1".into(),
                connection_id: None,
                saved_query_id: None,
                notebook_id: None,
                output_ref: None,
            },
            TabState {
                id: "t2".into(),
                title: "Query 2".into(),
                sql: "".into(),
                connection_id: Some(conn.id.clone()),
                saved_query_id: None,
                notebook_id: None,
                output_ref: None,
            },
        ];
        ws.save_tabs(&tabs).unwrap();
        assert_eq!(ws.list_tabs().unwrap(), tabs);
        ws.save_tabs(&tabs[1..]).unwrap();
        assert_eq!(ws.list_tabs().unwrap().len(), 1);

        ws.set_setting("theme", &serde_json::json!("dark")).unwrap();
        assert_eq!(ws.get_setting("theme").unwrap(), Some(serde_json::json!("dark")));
        assert_eq!(ws.all_settings().unwrap().len(), 1);
    }

    #[test]
    fn reopen_keeps_data() {
        let path = std::env::temp_dir().join(format!("databrain-ws-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let ws = Workspace::open(&path).unwrap();
            ws.save_connection(profile("keep")).unwrap();
        }
        let ws = Workspace::open(&path).unwrap();
        assert_eq!(ws.list_connections().unwrap()[0].name, "keep");
        drop(ws);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    #[test]
    fn notes_export_import_round_trip() {
        use crate::knowledge::{ImportAction, ImportKind};
        let ws = Workspace::open_in_memory().unwrap();
        let a = ws.save_connection(profile("A")).unwrap();
        let b = ws.save_connection(profile("B")).unwrap();
        let note = |conn: &str, target: Option<&str>, body: &str| KnNote {
            id: String::new(),
            connection_id: conn.into(),
            target: target.map(String::from),
            body: body.into(),
            author: "user".into(),
            status: NoteStatus::Approved,
            created_at: 0,
            replaces: None,
        };
        ws.kn_save_note(note(&a.id, Some("public.orders"), "Orders with status 9 are test orders.")).unwrap();
        ws.kn_save_note(note(&a.id, None, "Active customer: has an order in the last 90 days")).unwrap();
        ws.kn_save_note(note(&a.id, None, "MRR = monthly recurring revenue")).unwrap();
        ws.kn_save_note(KnNote { status: NoteStatus::Proposed, ..note(&a.id, None, "pending, not exported") }).unwrap();
        // B already knows some of it.
        ws.kn_save_note(note(&b.id, None, "MRR = monthly recurring revenue")).unwrap();
        let old = ws.kn_save_note(note(&b.id, None, "Active customer: logged in this month")).unwrap();

        let file = ws.kn_export_notes(&a.id).unwrap();
        assert_eq!(file.notes.len(), 3, "approved notes only");
        let text = serde_json::to_string(&file).unwrap();
        let file: NotesFile = serde_json::from_str(&text).unwrap();
        assert_eq!(file.source.as_ref().unwrap().connection, "A");

        let plan = ws.kn_import_plan(&b.id, &file).unwrap();
        let kind = |body: &str| plan.iter().find(|p| p.incoming.body.starts_with(body)).unwrap();
        assert_eq!(kind("Orders").kind, ImportKind::New);
        assert_eq!(kind("MRR").kind, ImportKind::Same);
        let conflict = kind("Active");
        assert_eq!(conflict.kind, ImportKind::Conflict, "same glossary term, different text");
        assert_eq!(conflict.existing[0].id, old.id);

        let acts = vec![
            ImportAction { incoming: kind("Orders").incoming.clone(), action: "add".into(), existing_ids: vec![], body: None, target: None },
            ImportAction {
                incoming: conflict.incoming.clone(),
                action: "merge".into(),
                existing_ids: vec![old.id.clone()],
                body: Some("Active customer: logged in this month and has an order in the last 90 days".into()),
                target: None,
            },
        ];
        assert_eq!(ws.kn_import_apply(&b.id, &acts).unwrap(), 2);
        let notes = ws.kn_notes(&b.id).unwrap();
        assert_eq!(notes.len(), 3);
        assert!(notes.iter().all(|n| n.id != old.id), "merged note replaced the old one");
        assert!(notes.iter().any(|n| n.target.as_deref() == Some("public.orders")));
        assert!(ws.kn_import_plan(&b.id, &NotesFile { format: "x".into(), ..file.clone() }).is_err());
        // Importing again: nothing new.
        assert!(ws.kn_import_plan(&b.id, &ws.kn_export_notes(&b.id).unwrap()).unwrap().iter().all(|p| p.kind == ImportKind::Same));
    }

    #[test]
    fn approving_an_update_replaces_the_note() {
        let ws = Workspace::open_in_memory().unwrap();
        let a = ws.save_connection(profile("A")).unwrap();
        let base = KnNote { id: String::new(), connection_id: a.id.clone(), target: None, body: "status 2 = closed".into(), author: "user".into(), status: NoteStatus::Approved, created_at: 0, replaces: None };
        let old = ws.kn_save_note(base.clone()).unwrap();
        let upd = ws.kn_save_note(KnNote { body: "status 2 = closed, 3 = archived".into(), author: "ai".into(), status: NoteStatus::Proposed, replaces: Some(old.id.clone()), ..base }).unwrap();
        assert_eq!(ws.kn_notes(&a.id).unwrap().len(), 2, "old note kept while the update is pending");
        assert_eq!(ws.kn_notes(&a.id).unwrap().iter().find(|n| n.id == upd.id).unwrap().replaces.as_deref(), Some(old.id.as_str()));
        ws.kn_save_note(KnNote { status: NoteStatus::Approved, ..upd }).unwrap();
        let notes = ws.kn_notes(&a.id).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].body.contains("archived") && notes[0].replaces.is_none());
    }
}
