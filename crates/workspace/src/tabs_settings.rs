//! Open editor tabs and settings key/value storage.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Result, Workspace};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
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
    /// Manual commit: statements run in a transaction the user commits or
    /// rolls back (`Some(false)`); `None`/`Some(true)` = auto-commit.
    #[serde(default)]
    pub auto_commit: Option<bool>,
    /// The `.sql` file on disk the tab edits (Save writes it).
    #[serde(default)]
    pub file_path: Option<String>,
}

impl Workspace {
    // ---------------------------------------------------------------- tabs

    pub fn list_tabs(&self) -> Result<Vec<TabState>> {
        let c = self.conn.lock();
        let mut stmt =
            c.prepare("SELECT id, title, sql, connection_id, saved_query_id, notebook_id, output_ref, auto_commit, file_path FROM tabs ORDER BY order_idx")?;
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
                    auto_commit: r.get(7)?,
                    file_path: r.get(8)?,
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
                "INSERT INTO tabs (id, title, sql, connection_id, saved_query_id, order_idx, notebook_id, output_ref, auto_commit, file_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for (i, t) in tabs.iter().enumerate() {
                stmt.execute(params![
                    t.id,
                    t.title,
                    t.sql,
                    t.connection_id,
                    t.saved_query_id,
                    i as i64,
                    t.notebook_id,
                    t.output_ref,
                    t.auto_commit,
                    t.file_path
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---------------------------------------------------------------- settings

    pub fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let c = self.conn.lock();
        let v: Option<String> = c.query_row("SELECT value_json FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?;
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
