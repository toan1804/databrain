//! Connection profiles, their AI policy, and the `Workspace` connection CRUD.

use databrain_connector_core::ConnectionConfig;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Workspace, new_id, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum EnvTag {
    #[default]
    None,
    Dev,
    Staging,
    Prod,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
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
    #[ts(as = "f64")]
    pub created_at: i64,
    #[serde(default)]
    #[ts(as = "f64")]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
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
    /// MCP: what DataBrain would ask the user about (per "AI may run queries"
    /// and the write rules) runs on the strength of the MCP client's own tool
    /// approval, without DataBrain's question. Off: it is asked through the
    /// client (elicitation) or, when the client can't, refused. What needs
    /// the user's confirmation in the app is never approved over MCP.
    pub mcp_client_approves: bool,
    /// Notes the AI adds or updates are saved directly instead of waiting
    /// for review in Knowledge.
    pub auto_approve_notes: bool,
    /// Learn how tables join (and which are used most) from this
    /// connection's query history, and give that to the AI with the schema.
    pub learn_from_history: bool,
    /// Tables the AI may read: LIKE patterns (`%`, `_`; case-insensitive)
    /// matched against `catalog.schema.table`, `schema.table` and `table`.
    /// Empty = every table.
    pub read_allow: Vec<String>,
    /// Tables the AI may never read (wins over `read_allow`).
    pub read_deny: Vec<String>,
    /// Tables the AI may write (with writes allowed). Empty = every table.
    pub write_allow: Vec<String>,
    /// Tables the AI may never write.
    pub write_deny: Vec<String>,
    /// DuckDB: SQL from the AI or MCP may read and write files, use the
    /// network, load extensions and change settings. Off: its sessions run
    /// in a sandbox (`enable_external_access = false`, locked settings).
    pub external_access: bool,
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
            mcp_client_approves: false,
            auto_approve_notes: false,
            learn_from_history: true,
            read_allow: vec![],
            read_deny: vec![],
            write_allow: vec![],
            write_deny: vec![],
            external_access: false,
        }
    }
}

impl Workspace {
    // ---------------------------------------------------------------- connections

    pub fn list_connections(&self) -> Result<Vec<ConnectionProfile>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, name, config_json, color, env, folder_id, has_secret, created_at, updated_at, ai_policy_json \
             FROM connections ORDER BY sort_order, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], connection_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
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
        p.config = p.config.normalized();
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
        // Another server (or user): the cached explorer describes something
        // else, and what the AI may use was chosen for the old one.
        if self.meta_check_identity(&p.id, &p.config)? {
            tracing::info!(connection = %p.id, "points at another server: explorer cache, AI index and AI scope cleared");
        }
        self.get_connection(&p.id)
    }

    /// Delete a connection and the data that only belongs to it (AI
    /// knowledge: indexed metadata, notes, index state, what was learned from history). Saved queries,
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
        tx.execute("DELETE FROM kn_joins WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM kn_table_seen WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM kn_learn_state WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM meta_columns WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM meta_schemas WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM meta_catalogs WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM meta_conn_state WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM ai_scope WHERE connection_id = ?1", [id])?;
        tx.execute("DELETE FROM ai_tables WHERE connection_id = ?1", [id])?;
        tx.execute("UPDATE tabs SET connection_id = NULL WHERE connection_id = ?1", [id])?;
        tx.commit()?;
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
