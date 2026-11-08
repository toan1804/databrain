//! Knowledge learned from query history: how tables join, and which tables
//! are used. Rows are sightings (one per join or table, query fingerprint
//! and day), so counts are of distinct queries and distinct days: a job
//! that runs every five minutes counts once a day, not 288 times. No SQL is
//! kept here; a sighting points at the history entry it came from, which
//! may since have been deleted (the counts stay).

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::history::{HISTORY_COLUMNS, history_row};
use crate::{HistoryEntry, Origin, Result, Workspace, now_ms};

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum JoinStatus {
    /// Seen in history; given to the AI once seen often enough.
    Learned,
    /// Accepted by the user: given to the AI however often it was seen.
    Confirmed,
    /// Rejected by the user: never given to the AI, never learned again.
    Rejected,
}

impl JoinStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JoinStatus::Learned => "learned",
            JoinStatus::Confirmed => "confirmed",
            JoinStatus::Rejected => "rejected",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "confirmed" => JoinStatus::Confirmed,
            "rejected" => JoinStatus::Rejected,
            _ => JoinStatus::Learned,
        }
    }
}

/// Whether a documented join may be given to the AI yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum DocState {
    /// Recorded by the assistant; waits for the user (unless the
    /// connection saves AI notes directly).
    Proposed,
    /// Recorded by the user, approved by them, or saved directly.
    Approved,
}

impl DocState {
    pub fn as_str(self) -> &'static str {
        match self {
            DocState::Proposed => "proposed",
            DocState::Approved => "approved",
        }
    }
}

/// Where a join is written down: a data dictionary, a spec, the user saying
/// so. A documented join doesn't need to be seen in queries.
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct JoinDoc {
    /// Where it is written, e.g. `orders spec §3` or `the user`.
    pub source: String,
    /// `user`, `ai` (DataBrain's assistant) or `mcp` (an MCP client).
    pub by: String,
    pub state: DocState,
    #[ts(as = "f64")]
    pub at: i64,
}

/// What [`Workspace::document_join`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Documented {
    /// A new join, or one known only from history, is now documented.
    Added,
    /// It was documented already; its source is updated.
    Updated,
    /// The user rejected it; nothing changed.
    Rejected,
}

/// A join seen in history: `left_table.left_columns[i] =
/// right_table.right_columns[i]`. Stored once per pair of sides, in a
/// canonical order; `kinds` are as seen from the left side.
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LearnedJoin {
    #[ts(as = "f64")]
    pub id: i64,
    pub left_table: String,
    pub left_columns: Vec<String>,
    pub right_table: String,
    pub right_columns: Vec<String>,
    pub status: JoinStatus,
    /// The same key is a declared foreign key.
    pub declared: bool,
    /// Distinct queries (fingerprints) run by the user or jobs.
    #[ts(as = "f64")]
    pub queries: i64,
    /// Distinct days it was used on by the user or jobs.
    #[ts(as = "f64")]
    pub days: i64,
    /// Distinct queries the assistant ran itself (AI panel, MCP): shown,
    /// never counted as evidence.
    #[ts(as = "f64")]
    pub ai_queries: i64,
    /// Join kind (`inner`, `left`, …) → distinct queries, user and jobs.
    #[ts(as = "BTreeMap<String, f64>")]
    pub kinds: BTreeMap<String, i64>,
    #[ts(as = "f64")]
    pub first_seen: i64,
    /// Last use by the user or a job; `None` when only the assistant used it.
    #[ts(as = "Option<f64>")]
    pub last_seen: Option<i64>,
    /// Written down somewhere (not only learned from queries).
    pub documented: Option<JoinDoc>,
}

/// One join found in one history entry.
#[derive(Debug, Clone)]
pub struct JoinSighting {
    pub left_table: String,
    pub left_columns: Vec<String>,
    pub right_table: String,
    pub right_columns: Vec<String>,
    /// `inner`, `left`, `right`, `full`, `semi`, `anti`, `where`.
    pub kind: String,
    /// Matches a declared foreign key.
    pub declared: bool,
    pub fingerprint: String,
    pub history_id: i64,
    /// When the query ran (ms).
    pub at: i64,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct TableUsage {
    pub table: String,
    /// Distinct queries by the user or jobs.
    #[ts(as = "f64")]
    pub queries: i64,
    #[ts(as = "f64")]
    pub days: i64,
    #[ts(as = "f64")]
    pub last_seen: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LearnState {
    /// History entries up to this id have been learned from.
    #[ts(as = "f64")]
    pub last_history_id: i64,
    #[ts(as = "f64")]
    pub learned_at: i64,
    pub error: Option<String>,
}

fn assistant(o: Origin) -> bool {
    matches!(o, Origin::Ai | Origin::Mcp)
}

fn flip(kind: &str) -> &str {
    match kind {
        "left" => "right",
        "right" => "left",
        k => k,
    }
}

/// The two sides in canonical order (`a ⋈ b` and `b ⋈ a` are one join);
/// `true` when they were swapped.
type JoinSide<'a> = (&'a str, &'a [String]);

fn canonical<'a>(l: JoinSide<'a>, r: JoinSide<'a>) -> (JoinSide<'a>, JoinSide<'a>, bool) {
    if (r.0, r.1) < (l.0, l.1) { (r, l, true) } else { (l, r, false) }
}

impl Workspace {
    /// Record a join seen in a history entry. The two sides are put in a
    /// canonical order (so `a ⋈ b` and `b ⋈ a` are one join, the kind
    /// flipped to match). Returns the join's id. A rejected join keeps
    /// counting (the UI shows it is still used) but stays rejected.
    pub fn record_join(&self, connection_id: &str, s: &JoinSighting) -> Result<i64> {
        let ((lt, lc), (rt, rc), swapped) = canonical((&s.left_table, &s.left_columns), (&s.right_table, &s.right_columns));
        let kind = if swapped { flip(&s.kind) } else { s.kind.as_str() };
        let (lc, rc) = (serde_json::to_string(lc)?, serde_json::to_string(rc)?);
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO kn_joins (connection_id, left_table, left_cols, right_table, right_cols, declared, first_seen) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (connection_id, left_table, left_cols, right_table, right_cols) \
             DO UPDATE SET declared = excluded.declared, first_seen = min(first_seen, excluded.first_seen)",
            params![connection_id, lt, lc, rt, rc, s.declared, s.at],
        )?;
        let id: i64 = c.query_row(
            "SELECT id FROM kn_joins WHERE connection_id = ?1 AND left_table = ?2 AND left_cols = ?3 AND right_table = ?4 AND right_cols = ?5",
            params![connection_id, lt, lc, rt, rc],
            |r| r.get(0),
        )?;
        // One row per query and day; the latest run of it is the evidence.
        c.execute(
            "INSERT INTO kn_join_seen (join_id, fingerprint, day, ai, kind, history_id, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (join_id, fingerprint, day, ai) DO UPDATE SET \
               history_id = max(history_id, excluded.history_id), at = max(at, excluded.at), kind = excluded.kind",
            params![id, s.fingerprint, s.at.div_euclid(DAY_MS), assistant(s.origin), kind, s.history_id, s.at],
        )?;
        Ok(id)
    }

    /// Record a join that is written down (`source`): by the user, the
    /// assistant (`ai`) or an MCP client (`mcp`). `approved` false leaves it
    /// for the user to review. A join the user rejected stays rejected.
    #[allow(clippy::too_many_arguments)]
    pub fn document_join(
        &self,
        connection_id: &str,
        left_table: &str,
        left_columns: &[String],
        right_table: &str,
        right_columns: &[String],
        source: &str,
        by: &str,
        approved: bool,
    ) -> Result<(i64, Documented)> {
        if left_columns.is_empty() || left_columns.len() != right_columns.len() {
            return Err(crate::Error::Invalid("a join needs the same number of columns on both sides".into()));
        }
        let ((lt, lc), (rt, rc), _) = canonical((left_table, left_columns), (right_table, right_columns));
        let (lc, rc) = (serde_json::to_string(lc)?, serde_json::to_string(rc)?);
        let state = if approved { DocState::Approved } else { DocState::Proposed };
        let c = self.conn.lock();
        let found: Option<(i64, String, Option<String>, Option<String>)> = c
            .query_row(
                "SELECT id, status, doc_source, doc_state FROM kn_joins WHERE connection_id = ?1 AND left_table = ?2 AND left_cols = ?3 AND right_table = ?4 AND right_cols = ?5",
                params![connection_id, lt, lc, rt, rc],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let now = now_ms();
        match found {
            Some((id, status, ..)) if JoinStatus::parse(&status) == JoinStatus::Rejected => Ok((id, Documented::Rejected)),
            Some((id, _, old, old_state)) => {
                // Approved stays approved when the assistant documents it again.
                let keep = old_state.as_deref() == Some("approved") && !approved;
                c.execute(
                    "UPDATE kn_joins SET doc_source = ?2, doc_by = ?3, doc_state = ?4, doc_at = ?5 WHERE id = ?1",
                    params![id, source, by, if keep { "approved" } else { state.as_str() }, now],
                )?;
                Ok((id, if old.is_some() { Documented::Updated } else { Documented::Added }))
            }
            None => {
                c.execute(
                    "INSERT INTO kn_joins (connection_id, left_table, left_cols, right_table, right_cols, first_seen, doc_source, doc_by, doc_state, doc_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?6)",
                    params![connection_id, lt, lc, rt, rc, now, source, by, state.as_str()],
                )?;
                Ok((c.last_insert_rowid(), Documented::Added))
            }
        }
    }

    /// The user's answer to a documented join the assistant proposed:
    /// approve it, or drop what was documented (a join also seen in
    /// queries stays, learned from them).
    pub fn review_documented_join(&self, id: i64, approve: bool) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let n = if approve {
            tx.execute("UPDATE kn_joins SET doc_state = 'approved' WHERE id = ?1 AND doc_source IS NOT NULL", [id])?
        } else {
            let seen: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM kn_join_seen WHERE join_id = ?1)", [id], |r| r.get(0))?;
            if seen {
                tx.execute("UPDATE kn_joins SET doc_source = NULL, doc_by = NULL, doc_state = NULL, doc_at = NULL WHERE id = ?1", [id])?
            } else {
                tx.execute("DELETE FROM kn_joins WHERE id = ?1", [id])?
            }
        };
        tx.commit()?;
        if n == 0 {
            return Err(crate::Error::NotFound(format!("documented join {id}")));
        }
        Ok(())
    }

    /// Record that a query read `table`. Only the user's and jobs' queries
    /// count; the assistant's are ignored.
    pub fn record_table_use(&self, connection_id: &str, table: &str, fingerprint: &str, at: i64, origin: Origin) -> Result<()> {
        if assistant(origin) {
            return Ok(());
        }
        self.conn.lock().execute(
            "INSERT INTO kn_table_seen (connection_id, table_name, fingerprint, day, at) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (connection_id, table_name, fingerprint, day) DO UPDATE SET at = max(at, excluded.at)",
            params![connection_id, table, fingerprint, at.div_euclid(DAY_MS), at],
        )?;
        Ok(())
    }

    /// Every learned join of a connection, most used first.
    pub fn learned_joins(&self, connection_id: &str) -> Result<Vec<LearnedJoin>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT j.id, j.left_table, j.left_cols, j.right_table, j.right_cols, j.status, j.declared, j.first_seen, \
               (SELECT count(DISTINCT fingerprint) FROM kn_join_seen s WHERE s.join_id = j.id AND s.ai = 0), \
               (SELECT count(DISTINCT day) FROM kn_join_seen s WHERE s.join_id = j.id AND s.ai = 0), \
               (SELECT count(DISTINCT fingerprint) FROM kn_join_seen s WHERE s.join_id = j.id AND s.ai = 1), \
               (SELECT max(at) FROM kn_join_seen s WHERE s.join_id = j.id AND s.ai = 0), \
               j.doc_source, j.doc_by, j.doc_state, j.doc_at \
             FROM kn_joins j WHERE j.connection_id = ?1",
        )?;
        let mut joins: Vec<LearnedJoin> = stmt
            .query_map([connection_id], |r| {
                let cols = |i: usize| -> rusqlite::Result<Vec<String>> { Ok(serde_json::from_str(&r.get::<_, String>(i)?).unwrap_or_default()) };
                Ok(LearnedJoin {
                    id: r.get(0)?,
                    left_table: r.get(1)?,
                    left_columns: cols(2)?,
                    right_table: r.get(3)?,
                    right_columns: cols(4)?,
                    status: JoinStatus::parse(&r.get::<_, String>(5)?),
                    declared: r.get(6)?,
                    first_seen: r.get(7)?,
                    queries: r.get(8)?,
                    days: r.get(9)?,
                    ai_queries: r.get(10)?,
                    last_seen: r.get(11)?,
                    kinds: BTreeMap::new(),
                    documented: match r.get::<_, Option<String>>(12)? {
                        Some(source) => Some(JoinDoc {
                            source,
                            by: r.get::<_, Option<String>>(13)?.unwrap_or_default(),
                            state: if r.get::<_, Option<String>>(14)?.as_deref() == Some("approved") { DocState::Approved } else { DocState::Proposed },
                            at: r.get::<_, Option<i64>>(15)?.unwrap_or_default(),
                        }),
                        None => None,
                    },
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut stmt = c.prepare(
            "SELECT s.join_id, s.kind, count(DISTINCT s.fingerprint) FROM kn_join_seen s JOIN kn_joins j ON j.id = s.join_id \
             WHERE j.connection_id = ?1 AND s.ai = 0 GROUP BY s.join_id, s.kind",
        )?;
        let kinds = stmt.query_map([connection_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
        for k in kinds {
            let (id, kind, n) = k?;
            if let Some(j) = joins.iter_mut().find(|j| j.id == id) {
                j.kinds.insert(kind, n);
            }
        }
        joins.sort_by(|a, b| (b.queries, b.days, b.last_seen).cmp(&(a.queries, a.days, a.last_seen)).then(a.id.cmp(&b.id)));
        Ok(joins)
    }

    pub fn set_join_status(&self, id: i64, status: JoinStatus) -> Result<()> {
        let n = self.conn.lock().execute("UPDATE kn_joins SET status = ?2 WHERE id = ?1", params![id, status.as_str()])?;
        if n == 0 {
            return Err(crate::Error::NotFound(format!("learned join {id}")));
        }
        Ok(())
    }

    /// History entries a join was seen in that are still kept, latest
    /// first; the user's and jobs' before the assistant's.
    pub fn join_evidence(&self, join_id: i64, limit: u32) -> Result<Vec<HistoryEntry>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(&format!(
            "SELECT {HISTORY_COLUMNS} FROM kn_join_seen s JOIN query_history h ON h.id = s.history_id \
             LEFT JOIN connections c ON c.id = h.connection_id \
             WHERE s.join_id = ?1 ORDER BY s.ai, h.id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![join_id, limit], history_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Tables the user's and jobs' queries read, most used first.
    pub fn table_usage(&self, connection_id: &str) -> Result<Vec<TableUsage>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT table_name, count(DISTINCT fingerprint), count(DISTINCT day), max(at) FROM kn_table_seen \
             WHERE connection_id = ?1 GROUP BY table_name ORDER BY 2 DESC, 3 DESC, 4 DESC, 1",
        )?;
        let rows = stmt
            .query_map([connection_id], |r| Ok(TableUsage { table: r.get(0)?, queries: r.get(1)?, days: r.get(2)?, last_seen: r.get(3)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn learn_state(&self, connection_id: &str) -> Result<Option<LearnState>> {
        let c = self.conn.lock();
        Ok(c.query_row("SELECT last_history_id, learned_at, error FROM kn_learn_state WHERE connection_id = ?1", [connection_id], |r| {
            Ok(LearnState { last_history_id: r.get(0)?, learned_at: r.get(1)?, error: r.get(2)? })
        })
        .optional()?)
    }

    /// History entries have been learned from up to `last_history_id`.
    pub fn set_learn_state(&self, connection_id: &str, last_history_id: i64, error: Option<&str>) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO kn_learn_state (connection_id, last_history_id, learned_at, error) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (connection_id) DO UPDATE SET last_history_id = excluded.last_history_id, learned_at = excluded.learned_at, error = excluded.error",
            params![connection_id, last_history_id, now_ms(), error],
        )?;
        Ok(())
    }

    /// The connection's history entries after `after_id`, oldest first.
    pub fn history_since(&self, connection_id: &str, after_id: i64, limit: u32) -> Result<Vec<HistoryEntry>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(&format!(
            "SELECT {HISTORY_COLUMNS} FROM query_history h LEFT JOIN connections c ON c.id = h.connection_id \
             WHERE h.connection_id = ?1 AND h.id > ?2 ORDER BY h.id LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![connection_id, after_id, limit], history_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Connections with history not learned from yet (one cheap query, for
    /// a periodic check).
    pub fn connections_to_learn(&self) -> Result<Vec<String>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT h.connection_id FROM query_history h JOIN connections c ON c.id = h.connection_id \
             LEFT JOIN kn_learn_state s ON s.connection_id = h.connection_id \
             GROUP BY h.connection_id HAVING max(h.id) > coalesce(max(s.last_history_id), 0) ORDER BY h.connection_id",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Forget everything learned for a connection (or every connection),
    /// rejections included. The history already there is not learned from
    /// again; new runs are. Documented joins stay (what was learned about
    /// their use goes).
    pub fn forget_learned(&self, connection_id: Option<&str>) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let cond = if connection_id.is_some() { "connection_id = ?1" } else { "?1 IS NULL" };
        tx.execute(&format!("DELETE FROM kn_joins WHERE ({cond}) AND doc_source IS NULL"), [connection_id])?;
        tx.execute(&format!("DELETE FROM kn_join_seen WHERE join_id IN (SELECT id FROM kn_joins WHERE {cond})"), [connection_id])?;
        tx.execute(&format!("UPDATE kn_joins SET status = 'learned' WHERE {cond}"), [connection_id])?;
        tx.execute(&format!("DELETE FROM kn_table_seen WHERE {cond}"), [connection_id])?;
        tx.execute(
            &format!(
                "INSERT INTO kn_learn_state (connection_id, last_history_id, learned_at) \
                 SELECT connection_id, max(id), ?2 FROM query_history WHERE connection_id IS NOT NULL AND ({cond}) GROUP BY connection_id \
                 ON CONFLICT (connection_id) DO UPDATE SET last_history_id = excluded.last_history_id, learned_at = excluded.learned_at, error = NULL"
            ),
            params![connection_id, now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every `(schema, name)` of a table or view called `name` (any case)
    /// in the knowledge index or the explorer cache, one per schema.
    pub fn tables_named(&self, connection_id: &str, name: &str) -> Result<Vec<(String, String)>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT schema_name, name FROM kn_objects WHERE connection_id = ?1 AND lower(name) = lower(?2) \
             UNION ALL \
             SELECT schema_name, name FROM meta_objects WHERE connection_id = ?1 AND lower(name) = lower(?2) \
               AND kind NOT IN ('function', 'procedure', 'package', 'sequence', 'other')",
        )?;
        let mut out: Vec<(String, String)> = Vec::new();
        for r in stmt.query_map(params![connection_id, name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (schema, n) = r?;
            if !out.iter().any(|(s, _)| s.eq_ignore_ascii_case(&schema)) {
                out.push((schema, n));
            }
        }
        Ok(out)
    }

    /// Drop sightings older than `days` (scores decay; very old ones no longer matter).
    pub fn prune_learned(&self, days: u32) -> Result<()> {
        let cutoff = (now_ms() - i64::from(days) * DAY_MS).div_euclid(DAY_MS);
        let c = self.conn.lock();
        c.execute("DELETE FROM kn_join_seen WHERE day < ?1", [cutoff])?;
        c.execute("DELETE FROM kn_table_seen WHERE day < ?1", [cutoff])?;
        // A join nobody has used since (not one the user decided on, nor a documented one).
        c.execute(
            "DELETE FROM kn_joins WHERE status = 'learned' AND doc_source IS NULL AND NOT EXISTS (SELECT 1 FROM kn_join_seen s WHERE s.join_id = kn_joins.id)",
            [],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AiPolicy, ConnectionProfile, EnvTag, NewHistory};
    use databrain_auth::AuthMethod;
    use databrain_connector_core::{ConnectionConfig, ConnectorKind};

    fn setup() -> (Workspace, String) {
        let ws = Workspace::open_in_memory().unwrap();
        let p = ws
            .save_connection(ConnectionProfile {
                id: String::new(),
                name: "db".into(),
                config: ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: "me".into() }),
                color: None,
                env: EnvTag::Dev,
                folder_id: None,
                has_secret: false,
                ai_policy: AiPolicy::default(),
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        (ws, p.id)
    }

    fn run(ws: &Workspace, conn: &str, sql: &str, origin: Origin) -> i64 {
        ws.add_history(NewHistory { origin, connection_id: Some(conn.into()), sql: sql.into(), ..Default::default() }).unwrap()
    }

    fn seen(left: (&str, &str), right: (&str, &str), kind: &str, fp: &str, history_id: i64, day: i64, origin: Origin) -> JoinSighting {
        JoinSighting {
            left_table: left.0.into(),
            left_columns: vec![left.1.into()],
            right_table: right.0.into(),
            right_columns: vec![right.1.into()],
            kind: kind.into(),
            declared: false,
            fingerprint: fp.into(),
            history_id,
            at: day * DAY_MS + 1000,
            origin,
        }
    }

    const ORD: (&str, &str) = ("sales.orders", "customer_id");
    const CUS: (&str, &str) = ("sales.customers", "id");

    #[test]
    fn both_directions_are_one_join_with_the_kind_flipped() {
        let (ws, c) = setup();
        let a = ws.record_join(&c, &seen(ORD, CUS, "left", "f1", 1, 100, Origin::User)).unwrap();
        let b = ws.record_join(&c, &seen(CUS, ORD, "right", "f2", 2, 100, Origin::User)).unwrap();
        assert_eq!(a, b);
        let j = &ws.learned_joins(&c).unwrap()[0];
        // Canonical order: `sales.customers` < `sales.orders`.
        assert_eq!((j.left_table.as_str(), j.right_table.as_str()), ("sales.customers", "sales.orders"));
        assert_eq!(j.kinds, BTreeMap::from([("right".to_string(), 2)]));
        assert_eq!((j.queries, j.days, j.status), (2, 1, JoinStatus::Learned));
    }

    #[test]
    fn counts_are_distinct_queries_and_days_and_the_assistant_is_not_counted() {
        let (ws, c) = setup();
        // A job: the same query many times a day, on two days.
        for (i, day) in [100, 100, 100, 101].into_iter().enumerate() {
            ws.record_join(&c, &seen(ORD, CUS, "inner", "job", i as i64 + 1, day, Origin::Job)).unwrap();
        }
        // The assistant's own runs.
        ws.record_join(&c, &seen(ORD, CUS, "inner", "ai1", 10, 102, Origin::Ai)).unwrap();
        ws.record_join(&c, &seen(ORD, CUS, "inner", "ai2", 11, 103, Origin::Mcp)).unwrap();
        let j = &ws.learned_joins(&c).unwrap()[0];
        assert_eq!((j.queries, j.days, j.ai_queries), (1, 2, 2));
        assert_eq!(j.last_seen, Some(101 * DAY_MS + 1000));
        // Only the assistant: listed, with no evidence of use.
        ws.record_join(&c, &seen(("a.x", "k"), ("a.y", "k"), "inner", "ai3", 12, 103, Origin::Ai)).unwrap();
        let only_ai = ws.learned_joins(&c).unwrap().into_iter().find(|j| j.left_table == "a.x").unwrap();
        assert_eq!((only_ai.queries, only_ai.days, only_ai.ai_queries, only_ai.last_seen), (0, 0, 1, None));
    }

    #[test]
    fn composite_keys_and_most_used_first() {
        let (ws, c) = setup();
        let mut s = seen(("sales.orders", "id"), ("sales.order_items", "order_id"), "inner", "f1", 1, 5, Origin::User);
        s.left_columns.push("line_no".into());
        s.right_columns.push("line_no".into());
        ws.record_join(&c, &s).unwrap();
        ws.record_join(&c, &seen(ORD, CUS, "inner", "f2", 2, 5, Origin::User)).unwrap();
        ws.record_join(&c, &seen(ORD, CUS, "inner", "f3", 3, 6, Origin::User)).unwrap();
        let all = ws.learned_joins(&c).unwrap();
        assert_eq!(all[0].right_table, "sales.orders");
        // `sales.order_items` < `sales.orders`: the item side is left.
        assert_eq!(all[1].left_columns, ["order_id", "line_no"]);
        assert_eq!(all[1].right_columns, ["id", "line_no"]);
    }

    #[test]
    fn rejected_stays_rejected_while_it_keeps_counting() {
        let (ws, c) = setup();
        let id = ws.record_join(&c, &seen(ORD, CUS, "inner", "f1", 1, 1, Origin::User)).unwrap();
        ws.set_join_status(id, JoinStatus::Rejected).unwrap();
        ws.record_join(&c, &seen(ORD, CUS, "inner", "f2", 2, 2, Origin::User)).unwrap();
        let j = &ws.learned_joins(&c).unwrap()[0];
        assert_eq!((j.status, j.queries), (JoinStatus::Rejected, 2));
        ws.set_join_status(id, JoinStatus::Confirmed).unwrap();
        assert_eq!(ws.learned_joins(&c).unwrap()[0].status, JoinStatus::Confirmed);
        assert!(ws.set_join_status(999, JoinStatus::Confirmed).is_err());
    }

    #[test]
    fn evidence_is_kept_history_latest_first_and_survives_as_counts() {
        let (ws, c) = setup();
        let h1 = run(&ws, &c, "select 1", Origin::User);
        let h2 = run(&ws, &c, "select 2", Origin::Ai);
        let h3 = run(&ws, &c, "select 3", Origin::User);
        let id = ws.record_join(&c, &seen(ORD, CUS, "inner", "f1", h1, 1, Origin::User)).unwrap();
        ws.record_join(&c, &seen(ORD, CUS, "inner", "f2", h2, 1, Origin::Ai)).unwrap();
        ws.record_join(&c, &seen(ORD, CUS, "inner", "f3", h3, 2, Origin::User)).unwrap();
        let ev: Vec<i64> = ws.join_evidence(id, 5).unwrap().into_iter().map(|h| h.id).collect();
        assert_eq!(ev, [h3, h1, h2]);
        // History cleared: no evidence to show, the join and its counts stay.
        ws.clear_history().unwrap();
        assert!(ws.join_evidence(id, 5).unwrap().is_empty());
        assert_eq!(ws.learned_joins(&c).unwrap()[0].queries, 2);
    }

    #[test]
    fn table_usage_counts_user_and_jobs_only() {
        let (ws, c) = setup();
        ws.record_table_use(&c, "sales.orders", "f1", 10 * DAY_MS, Origin::User).unwrap();
        ws.record_table_use(&c, "sales.orders", "f1", 10 * DAY_MS + 5, Origin::Job).unwrap();
        ws.record_table_use(&c, "sales.orders", "f2", 11 * DAY_MS, Origin::User).unwrap();
        ws.record_table_use(&c, "sales.customers", "f2", 11 * DAY_MS, Origin::User).unwrap();
        ws.record_table_use(&c, "sales.secrets", "f9", 11 * DAY_MS, Origin::Ai).unwrap();
        let u = ws.table_usage(&c).unwrap();
        assert_eq!(u.iter().map(|t| (t.table.as_str(), t.queries, t.days)).collect::<Vec<_>>(), [("sales.orders", 2, 2), ("sales.customers", 1, 1)]);
    }

    #[test]
    fn cursor_and_connections_to_learn() {
        let (ws, c) = setup();
        assert!(ws.connections_to_learn().unwrap().is_empty());
        let h1 = run(&ws, &c, "select 1", Origin::User);
        let h2 = run(&ws, &c, "select 2", Origin::User);
        assert_eq!(ws.connections_to_learn().unwrap(), std::slice::from_ref(&c));
        assert_eq!(ws.history_since(&c, 0, 10).unwrap().iter().map(|h| h.id).collect::<Vec<_>>(), [h1, h2]);
        assert_eq!(ws.history_since(&c, h1, 10).unwrap().len(), 1);
        ws.set_learn_state(&c, h2, None).unwrap();
        assert!(ws.connections_to_learn().unwrap().is_empty());
        assert_eq!(ws.learn_state(&c).unwrap().unwrap().last_history_id, h2);
        run(&ws, &c, "select 3", Origin::User);
        assert_eq!(ws.connections_to_learn().unwrap(), std::slice::from_ref(&c));
    }

    #[test]
    fn forget_drops_everything_and_does_not_relearn_old_history() {
        let (ws, c) = setup();
        let h = run(&ws, &c, "select 1", Origin::User);
        let id = ws.record_join(&c, &seen(ORD, CUS, "inner", "f1", h, 1, Origin::User)).unwrap();
        ws.set_join_status(id, JoinStatus::Rejected).unwrap();
        ws.record_table_use(&c, "sales.orders", "f1", DAY_MS, Origin::User).unwrap();
        ws.forget_learned(Some(&c)).unwrap();
        assert!(ws.learned_joins(&c).unwrap().is_empty());
        assert!(ws.table_usage(&c).unwrap().is_empty());
        assert!(ws.connections_to_learn().unwrap().is_empty());
        // All connections.
        ws.record_join(&c, &seen(ORD, CUS, "inner", "f1", h, 1, Origin::User)).unwrap();
        ws.forget_learned(None).unwrap();
        assert!(ws.learned_joins(&c).unwrap().is_empty());
    }

    #[test]
    fn documented_joins_need_no_queries_and_survive_forget_and_prune() {
        let (ws, c) = setup();
        let cols = |c: &str| vec![c.to_string()];
        // Recorded from the other side: one join, in canonical order.
        let (id, what) = ws.document_join(&c, CUS.0, &cols(CUS.1), ORD.0, &cols(ORD.1), "orders spec §3", "ai", false).unwrap();
        assert_eq!(what, Documented::Added);
        let j = &ws.learned_joins(&c).unwrap()[0];
        assert_eq!((j.left_table.as_str(), j.right_table.as_str(), j.queries), ("sales.customers", "sales.orders", 0));
        let doc = j.documented.as_ref().unwrap();
        assert_eq!((doc.source.as_str(), doc.by.as_str(), doc.state), ("orders spec §3", "ai", DocState::Proposed));
        // Seen in a query too: still one join, now with use.
        let h = run(&ws, &c, "select 1", Origin::User);
        assert_eq!(ws.record_join(&c, &seen(ORD, CUS, "inner", "f1", h, 1, Origin::User)).unwrap(), id);
        let j = &ws.learned_joins(&c).unwrap()[0];
        assert_eq!((j.queries, j.documented.as_ref().map(|d| d.state)), (1, Some(DocState::Proposed)));
        // Approved; documenting it again (proposed) keeps it approved.
        ws.review_documented_join(id, true).unwrap();
        assert_eq!(ws.document_join(&c, ORD.0, &cols(ORD.1), CUS.0, &cols(CUS.1), "data dictionary", "mcp", false).unwrap(), (id, Documented::Updated));
        let doc = ws.learned_joins(&c).unwrap()[0].documented.clone().unwrap();
        assert_eq!((doc.source.as_str(), doc.state), ("data dictionary", DocState::Approved));
        // Forget and prune keep it (without its use).
        ws.forget_learned(Some(&c)).unwrap();
        ws.prune_learned(0).unwrap();
        let j = &ws.learned_joins(&c).unwrap()[0];
        assert_eq!((j.id, j.queries), (id, 0));
        // Rejected stays rejected.
        ws.set_join_status(id, JoinStatus::Rejected).unwrap();
        assert_eq!(ws.document_join(&c, ORD.0, &cols(ORD.1), CUS.0, &cols(CUS.1), "again", "ai", true).unwrap(), (id, Documented::Rejected));
        // Bad shape.
        assert!(ws.document_join(&c, ORD.0, &[], CUS.0, &[], "x", "ai", true).is_err());
    }

    #[test]
    fn discarding_a_proposed_join_keeps_what_queries_taught() {
        let (ws, c) = setup();
        let cols = |c: &str| vec![c.to_string()];
        let (only_doc, _) = ws.document_join(&c, "a.x", &cols("k"), "a.y", &cols("k"), "spec", "ai", false).unwrap();
        let h = run(&ws, &c, "select 1", Origin::User);
        let used = ws.record_join(&c, &seen(ORD, CUS, "inner", "f1", h, 1, Origin::User)).unwrap();
        ws.document_join(&c, ORD.0, &cols(ORD.1), CUS.0, &cols(CUS.1), "spec", "ai", false).unwrap();
        ws.review_documented_join(only_doc, false).unwrap();
        ws.review_documented_join(used, false).unwrap();
        let joins = ws.learned_joins(&c).unwrap();
        assert_eq!(joins.len(), 1);
        assert_eq!((joins[0].id, joins[0].documented.is_none()), (used, true));
        assert!(ws.review_documented_join(only_doc, true).is_err());
    }

    #[test]
    fn deleting_the_connection_deletes_what_was_learned() {
        let (ws, c) = setup();
        run(&ws, &c, "select 1", Origin::User);
        ws.record_join(&c, &seen(ORD, CUS, "inner", "f1", 1, 1, Origin::User)).unwrap();
        ws.record_table_use(&c, "sales.orders", "f1", DAY_MS, Origin::User).unwrap();
        ws.set_learn_state(&c, 1, None).unwrap();
        ws.delete_connection(&c).unwrap();
        let conn = ws.conn.lock();
        for t in ["kn_joins", "kn_join_seen", "kn_table_seen", "kn_learn_state"] {
            let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{t}");
        }
    }

    #[test]
    fn prune_keeps_decided_joins() {
        let (ws, c) = setup();
        let old = seen(ORD, CUS, "inner", "f1", 1, 1, Origin::User);
        let kept = ws.record_join(&c, &seen(("a.x", "k"), ("a.y", "k"), "inner", "f2", 2, 1, Origin::User)).unwrap();
        ws.record_join(&c, &old).unwrap();
        ws.set_join_status(kept, JoinStatus::Confirmed).unwrap();
        ws.prune_learned(365).unwrap();
        let left: Vec<i64> = ws.learned_joins(&c).unwrap().into_iter().map(|j| j.id).collect();
        assert_eq!(left, [kept]);
    }

    #[test]
    fn learning_is_on_by_default_also_for_saved_policies_without_the_field() {
        assert!(AiPolicy::default().learn_from_history);
        let p: AiPolicy = serde_json::from_str(r#"{"ai_enabled": true}"#).unwrap();
        assert!(p.learn_from_history);
    }
}
