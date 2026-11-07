//! Query history: entries, search, retention and clearing.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Origin, Result, Workspace, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct HistoryEntry {
    #[ts(as = "f64")]
    pub id: i64,
    #[serde(default)]
    pub origin: Origin,
    pub connection_id: Option<String>,
    pub connection_name: Option<String>,
    pub sql: String,
    #[ts(as = "f64")]
    pub started_at: i64,
    #[ts(as = "f64")]
    pub duration_ms: i64,
    #[ts(as = "Option<f64>")]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct HistoryQuery {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Return entries with `id < before` (pagination).
    #[serde(default)]
    #[ts(as = "Option<f64>")]
    pub before: Option<i64>,
    /// Leave out statements run by the assistant (AI panel and MCP clients).
    #[serde(default)]
    #[ts(as = "Option<bool>", optional)]
    pub exclude_ai: bool,
    /// Only entries that ended like this.
    #[serde(default)]
    #[ts(optional)]
    pub status: Option<RunStatus>,
}

/// Search words of a history search: whitespace-separated, `"…"` keeps a
/// phrase together; at most 8.
pub fn history_terms(search: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = search.trim();
    while !rest.is_empty() && out.len() < 8 {
        if let Some(q) = rest.strip_prefix('"') {
            let (phrase, after) = q.split_once('"').unwrap_or((q, ""));
            if !phrase.trim().is_empty() {
                out.push(phrase.to_string());
            }
            rest = after.trim_start();
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push(rest[..end].to_string());
            rest = rest[end..].trim_start();
        }
    }
    out
}

/// What "Clear history" deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(tag = "kind", content = "days", rename_all = "snake_case")]
pub enum HistoryClear {
    /// Everything.
    All,
    /// Statements run by the AI assistant and MCP clients.
    Ai,
    /// Entries older than this many days (keeps the recent ones).
    OlderThanDays(u32),
}

/// Setting: delete history older than this many days (0 = keep it all).
pub const HISTORY_RETENTION_SETTING: &str = "history_retention_days";
pub const HISTORY_RETENTION_DEFAULT_DAYS: u32 = 30;
/// Setting: delete AI conversations idle for longer than this many days (0 = keep them).
pub const AI_HISTORY_RETENTION_SETTING: &str = "ai_history_retention_days";

/// Maximum history rows kept; older rows are pruned on insert.
pub const HISTORY_LIMIT: i64 = 5000;

impl Workspace {
    // ---------------------------------------------------------------- history

    pub fn add_history(&self, mut h: NewHistory) -> Result<i64> {
        // Passwords and keys in the SQL (`CREATE USER … IDENTIFIED BY …`) are not kept.
        // The same redaction as the AI audit log: connection strings and
        // `name=value` secrets too (`ATTACH 'host=h password=…'`, `MASTER_PASSWORD=…`).
        h.sql = databrain_connector_core::sql::redact_text(&h.sql);
        h.error = h.error.map(|e| databrain_connector_core::sql::redact_text(&e));
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
        c.execute("DELETE FROM query_history WHERE id <= ?1 - ?2", params![id, HISTORY_LIMIT])?;
        Ok(id)
    }

    pub fn list_history(&self, q: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        let c = self.conn.lock();
        let limit = q.limit.unwrap_or(200).min(1000) as i64;
        // Every word must appear, in any order, in the SQL, the error or the
        // connection's name (not case-sensitive for ASCII).
        let terms: Vec<String> = history_terms(q.search.as_deref().unwrap_or(""))
            .into_iter()
            .map(|s| format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")))
            .collect();
        let mut sql = String::from(
            "SELECT h.id, h.connection_id, c.name, h.sql, h.started_at, h.duration_ms, h.rows, h.status, h.error, h.origin, h.output_handle, h.result_id \
             FROM query_history h LEFT JOIN connections c ON c.id = h.connection_id \
             WHERE (?1 IS NULL OR h.connection_id = ?1) \
               AND (?2 IS NULL OR h.id < ?2) \
               AND (?4 = 0 OR h.origin NOT IN ('ai', 'mcp')) \
               AND (?5 IS NULL OR h.status = ?5)",
        );
        for i in 0..terms.len() {
            let n = i + 6;
            sql.push_str(&format!(" AND (h.sql LIKE ?{n} ESCAPE '\\' OR h.error LIKE ?{n} ESCAPE '\\' OR c.name LIKE ?{n} ESCAPE '\\')"));
        }
        sql.push_str(" ORDER BY h.id DESC LIMIT ?3");
        let mut values: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(q.connection_id.clone()), Box::new(q.before), Box::new(limit), Box::new(q.exclude_ai), Box::new(q.status.map(RunStatus::as_str))];
        values.extend(terms.into_iter().map(|t| Box::new(t) as Box<dyn rusqlite::ToSql>));
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(values.iter().map(|v| v.as_ref())), |r| {
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

    /// Delete one history entry; returns its result id (to drop the result too).
    pub fn delete_history_entry(&self, id: i64) -> Result<Option<String>> {
        let c = self.conn.lock();
        let rid: Option<Option<String>> = c.query_row("SELECT result_id FROM query_history WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
        c.execute("DELETE FROM query_history WHERE id = ?1", params![id])?;
        Ok(rid.flatten())
    }

    /// Days of history kept (Settings → Queries); `None` = keep it all.
    pub fn history_retention_days(&self) -> Option<u32> {
        match self.get_setting(HISTORY_RETENTION_SETTING).ok().flatten() {
            None => Some(HISTORY_RETENTION_DEFAULT_DAYS),
            Some(v) => v.as_u64().map(|d| d as u32).filter(|d| *d > 0),
        }
    }

    /// Apply the retention setting; returns entries deleted and their result ids.
    pub fn prune_history(&self) -> Result<(usize, Vec<String>)> {
        match self.history_retention_days() {
            Some(days) => self.clear_history_results(HistoryClear::OlderThanDays(days)),
            None => Ok((0, vec![])),
        }
    }

    pub fn clear_history(&self) -> Result<()> {
        self.clear_history_by(HistoryClear::All).map(|_| ())
    }

    /// Delete part of the history; returns how many entries went.
    pub fn clear_history_by(&self, what: HistoryClear) -> Result<usize> {
        Ok(self.clear_history_results(what)?.0)
    }

    /// Like [`Self::clear_history_by`], also returning the result ids of the
    /// deleted entries (their outputs are dropped too).
    pub fn clear_history_results(&self, what: HistoryClear) -> Result<(usize, Vec<String>)> {
        let c = self.conn.lock();
        let (cond, cutoff) = match what {
            HistoryClear::All => ("1 = 1", 0),
            HistoryClear::Ai => ("origin IN ('ai', 'mcp')", 0),
            HistoryClear::OlderThanDays(days) => ("started_at < ?1", now_ms() - days as i64 * 86_400_000),
        };
        let uses_cutoff = matches!(what, HistoryClear::OlderThanDays(_));
        let sel = format!("SELECT result_id FROM query_history WHERE {cond} AND result_id IS NOT NULL");
        let mut stmt = c.prepare(&sel)?;
        let ids: Vec<String> = if uses_cutoff {
            stmt.query_map(params![cutoff], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        drop(stmt);
        let del = format!("DELETE FROM query_history WHERE {cond}");
        let n = if uses_cutoff { c.execute(&del, params![cutoff])? } else { c.execute(&del, [])? };
        Ok((n, ids))
    }
}
