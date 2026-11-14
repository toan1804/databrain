//! What the AI may use: the user's scope rules (`ai_scope`), the tables they
//! allow among those the explorer cache knows (`ai_tables`), and the one
//! reader AI code gets ([`AiCatalog`]), over the `ai_objects` view.
//!
//! Server facts (objects, columns, foreign keys) live once, in the explorer
//! cache (`meta_*`). A table the scope doesn't allow has no `ai_tables` row,
//! so nothing [`AiCatalog`] returns can name it: not search, not lookups,
//! not foreign-key paths. `ai_tables` and the search rows of objects
//! (`kn_fts`, source `object`) are recomputed in the same transaction as any
//! write that can change them (listings, describes, scope edits).
//!
//! The connection's table access rules (`AiPolicy::read_allow` /
//! `read_deny`) need each engine's default catalog and schema to match, so
//! the `ai` crate applies them on top of this reader.

use databrain_connector_core::ForeignKey;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::knowledge::fts_query;
use crate::{KnHit, KnObject, Result, Workspace};

/// One rule of a connection's AI scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct ScopeRule {
    /// A schema id (`main.sales`, `public`), `catalog.*` or `*`.
    pub schema: String,
    /// One table of that schema; empty for every table.
    #[serde(default)]
    pub table: String,
    /// Allow (`true`) or exclude.
    pub allow: bool,
}

/// How specific a rule is for `schema`.`table`; `None` when it doesn't match.
/// A table rule beats a schema id, which beats `catalog.*`, which beats `*`.
fn specificity(rule: &ScopeRule, schema: &str, table: &str) -> Option<u8> {
    let s = rule.schema.trim();
    let schema_level = if s == "*" {
        0
    } else if s.eq_ignore_ascii_case(schema) {
        2
    } else if let Some(c) = s.strip_suffix(".*")
        && schema.len() > c.len()
        && schema[..c.len()].eq_ignore_ascii_case(c)
        && schema.as_bytes()[c.len()] == b'.'
    {
        1
    } else {
        return None;
    };
    match rule.table.trim() {
        "" => Some(schema_level),
        t if schema_level == 2 && t.eq_ignore_ascii_case(table) => Some(3),
        _ => None,
    }
}

/// Whether the rules let the AI use `schema`.`table`: the most specific
/// matching rule decides, and an exclusion wins over an allowance of the
/// same level. No matching rule: no.
pub fn scope_allows(rules: &[ScopeRule], schema: &str, table: &str) -> bool {
    let mut best: Option<(u8, bool)> = None;
    for r in rules {
        if let Some(level) = specificity(r, schema, table) {
            best = match best {
                Some((l, a)) if l > level || (l == level && !a) => Some((l, a)),
                _ => Some((level, r.allow)),
            };
        }
    }
    best.is_some_and(|(_, allow)| allow)
}

/// Object kinds the AI can use (as `meta_objects.kind`).
const RELATIONS: &str = "('table', 'view', 'materialized_view', 'foreign_table')";

impl Workspace {
    /// The AI scope of a connection.
    pub fn ai_scope(&self, connection_id: &str) -> Result<Vec<ScopeRule>> {
        Self::scope_tx(&self.conn.lock(), connection_id)
    }

    fn scope_tx(c: &rusqlite::Connection, connection_id: &str) -> Result<Vec<ScopeRule>> {
        let mut st = c.prepare_cached("SELECT schema_rule, table_name, allow FROM ai_scope WHERE connection_id = ?1 ORDER BY schema_rule, table_name")?;
        let rows = st
            .query_map([connection_id], |r| Ok(ScopeRule { schema: r.get(0)?, table: r.get(1)?, allow: r.get(2)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Replace the AI scope of a connection; what the AI may use follows.
    pub fn ai_set_scope(&self, connection_id: &str, rules: &[ScopeRule]) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM ai_scope WHERE connection_id = ?1", [connection_id])?;
        for r in rules {
            Self::put_rule(&tx, connection_id, r)?;
        }
        Self::ai_refresh_tx(&tx, connection_id, None)?;
        tx.commit()?;
        Ok(())
    }

    /// Add or change one rule (allowing a schema the user indexes, a table
    /// the AI asked for).
    pub fn ai_put_rule(&self, connection_id: &str, rule: &ScopeRule) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        Self::put_rule(&tx, connection_id, rule)?;
        Self::ai_refresh_tx(&tx, connection_id, None)?;
        tx.commit()?;
        Ok(())
    }

    /// Remove one rule.
    pub fn ai_delete_rule(&self, connection_id: &str, schema: &str, table: &str) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute(
            "DELETE FROM ai_scope WHERE connection_id = ?1 AND schema_rule = ?2 AND lower(table_name) = lower(?3)",
            params![connection_id, schema.trim(), table.trim()],
        )?;
        Self::ai_refresh_tx(&tx, connection_id, None)?;
        tx.commit()?;
        Ok(())
    }

    /// The schema-level allow rules are exactly `schemas` (the scope chosen
    /// for indexing); per-table rules and exclusions stay.
    pub fn ai_set_schema_rules(&self, connection_id: &str, schemas: &[String]) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM ai_scope WHERE connection_id = ?1 AND table_name = '' AND allow = 1", [connection_id])?;
        for s in schemas {
            Self::put_rule(&tx, connection_id, &ScopeRule { schema: s.clone(), table: String::new(), allow: true })?;
        }
        Self::ai_refresh_tx(&tx, connection_id, None)?;
        tx.commit()?;
        Ok(())
    }

    /// Remove the schema-level allow rules of `schemas` (taken out of the
    /// index scope). The user's exclusions and single tables allowed on
    /// their own stay.
    pub fn ai_remove_schemas(&self, connection_id: &str, schemas: &[String]) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute(
            "DELETE FROM ai_scope WHERE connection_id = ?1 AND table_name = '' AND allow = 1 \
               AND lower(schema_rule) IN (SELECT lower(value) FROM json_each(?2))",
            params![connection_id, serde_json::to_string(schemas)?],
        )?;
        Self::ai_refresh_tx(&tx, connection_id, None)?;
        tx.commit()?;
        Ok(())
    }

    /// [`Self::put_rule`] unless a rule for the same schema and table
    /// exists, or a broader exclusion decides it (`main.*` excluded, then
    /// `main.hr` indexed): indexing never overrides what the user set.
    pub(crate) fn put_rule_if_absent(tx: &Transaction<'_>, connection_id: &str, r: &ScopeRule) -> Result<()> {
        if r.allow {
            let rules = Self::scope_tx(tx, connection_id)?;
            let (schema, table) = (r.schema.trim(), r.table.trim());
            if rules.iter().any(|x| specificity(x, schema, table).is_some()) && !scope_allows(&rules, schema, table) {
                return Ok(());
            }
        }
        tx.execute(
            "INSERT OR IGNORE INTO ai_scope (connection_id, schema_rule, table_name, allow) VALUES (?1, ?2, ?3, ?4)",
            params![connection_id, r.schema.trim(), r.table.trim(), r.allow],
        )?;
        Ok(())
    }

    /// The user excluded `schema`.`table` from the AI: the most specific
    /// rule matching it says no. (No rule at all is not an exclusion: the
    /// AI may ask to use such a table.)
    pub fn ai_excluded(&self, connection_id: &str, schema: &str, table: &str) -> Result<bool> {
        let rules = self.ai_scope(connection_id)?;
        let matched = rules.iter().any(|r| specificity(r, schema, table).is_some());
        Ok(matched && !scope_allows(&rules, schema, table))
    }

    pub(crate) fn put_rule(tx: &Transaction<'_>, connection_id: &str, r: &ScopeRule) -> Result<()> {
        tx.execute(
            "INSERT INTO ai_scope (connection_id, schema_rule, table_name, allow) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (connection_id, schema_rule, table_name) DO UPDATE SET allow = excluded.allow",
            params![connection_id, r.schema.trim(), r.table.trim(), r.allow],
        )?;
        Ok(())
    }

    /// Recompute what the AI may use of `schema` (every schema when `None`)
    /// and the search rows of those tables. Called inside the transaction of
    /// the write that changed them. Search rows are kept by rowid in
    /// `ai_tables` and rewritten only for tables whose search text changed,
    /// so a recompute never scans the search table.
    pub(crate) fn ai_refresh_tx(tx: &Transaction<'_>, connection_id: &str, schema: Option<&str>) -> Result<()> {
        let rules = Self::scope_tx(tx, connection_id)?;
        type Old = std::collections::HashMap<(String, String), (Option<i64>, Option<String>)>;
        let mut old: Old = {
            let mut st = tx.prepare_cached(
                "SELECT schema_name, name, fts_rowid, body_hash FROM ai_tables WHERE connection_id = ?1 AND (?2 IS NULL OR schema_name = ?2)",
            )?;
            st.query_map(params![connection_id, schema], |r| Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?))))?.collect::<rusqlite::Result<_>>()?
        };
        // No scope and nothing allowed before: nothing to do (the usual case
        // for connections the AI isn't used on).
        if rules.is_empty() && old.is_empty() {
            return Ok(());
        }
        let facts = Self::facts_tx(tx, connection_id, schema, None)?;
        let mut seen = std::collections::HashSet::new();
        for o in facts.into_iter().filter(|o| scope_allows(&rules, &o.schema, &o.name)) {
            let key = (o.schema.clone(), o.name.clone());
            if !seen.insert(key.clone()) {
                continue;
            }
            Self::put_search_row(tx, connection_id, &o, old.remove(&key))?;
        }
        // No longer allowed (or gone): their rows go.
        for ((s, n), (rowid, _)) in old {
            if let Some(r) = rowid {
                tx.execute("DELETE FROM kn_fts WHERE rowid = ?1", [r])?;
            }
            tx.execute("DELETE FROM ai_tables WHERE connection_id = ?1 AND schema_name = ?2 AND name = ?3", params![connection_id, s, n])?;
        }
        Ok(())
    }

    /// One allowed table's `ai_tables` row and search row: written when new,
    /// rewritten when its search text changed, left alone otherwise.
    fn put_search_row(tx: &Transaction<'_>, connection_id: &str, o: &KnObject, old: Option<(Option<i64>, Option<String>)>) -> Result<()> {
        let hash = o.hash();
        if let Some((Some(_), Some(h))) = &old
            && *h == hash
        {
            return Ok(());
        }
        if let Some((Some(r), _)) = old {
            tx.execute("DELETE FROM kn_fts WHERE rowid = ?1", [r])?;
        }
        tx.prepare_cached("INSERT INTO kn_fts (connection_id, source, ref, title, body) VALUES (?1, 'object', ?2, ?3, ?4)")?.execute(params![
            connection_id,
            o.full_name(),
            o.name,
            o.search_body()
        ])?;
        let rowid = tx.last_insert_rowid();
        tx.prepare_cached(
            "INSERT INTO ai_tables (connection_id, schema_name, name, fts_rowid, body_hash) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (connection_id, schema_name, name) DO UPDATE SET fts_rowid = excluded.fts_rowid, body_hash = excluded.body_hash",
        )?
        .execute(params![connection_id, o.schema, o.name, rowid, hash])?;
        Ok(())
    }

    /// What the explorer cache knows of a schema's tables (or one table),
    /// whether or not the AI may use them.
    fn facts_tx(c: &rusqlite::Connection, connection_id: &str, schema: Option<&str>, name: Option<&str>) -> Result<Vec<KnObject>> {
        let mut st = c.prepare_cached(&format!(
            "SELECT o.schema_name, o.name, o.kind, o.comment, o.row_estimate, m.columns_json, m.fks_json FROM meta_objects o \
             LEFT JOIN meta_columns m ON m.connection_id = o.connection_id AND m.schema_name = o.schema_name AND m.table_name = o.name \
             WHERE o.connection_id = ?1 AND (?2 IS NULL OR o.schema_name = ?2) AND (?3 IS NULL OR o.name = ?3) AND o.kind IN {RELATIONS} \
             ORDER BY o.schema_name, o.name, o.kind = 'table' DESC"
        ))?;
        let rows = st.query_map(params![connection_id, schema, name], Self::ai_object_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The search row of one table after its columns changed (a describe).
    pub(crate) fn ai_refresh_table_tx(tx: &Transaction<'_>, connection_id: &str, schema: &str, name: &str) -> Result<()> {
        let old: Option<(Option<i64>, Option<String>)> = tx
            .query_row(
                "SELECT fts_rowid, body_hash FROM ai_tables WHERE connection_id = ?1 AND schema_name = ?2 AND name = ?3",
                params![connection_id, schema, name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // Not allowed: nothing to index.
        let Some(old) = old else { return Ok(()) };
        if let Some(o) = Self::facts_tx(tx, connection_id, Some(schema), Some(name))?.into_iter().next() {
            Self::put_search_row(tx, connection_id, &o, Some(old))?;
        }
        Ok(())
    }

    /// Forget what the AI may use of a connection (its facts are gone); the
    /// scope stays.
    pub(crate) fn ai_forget_tx(c: &rusqlite::Connection, connection_id: &str) -> Result<()> {
        c.execute("DELETE FROM kn_fts WHERE rowid IN (SELECT fts_rowid FROM ai_tables WHERE connection_id = ?1 AND fts_rowid IS NOT NULL)", [connection_id])?;
        c.execute("DELETE FROM ai_tables WHERE connection_id = ?1", [connection_id])?;
        Ok(())
    }

    fn ai_object_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<KnObject> {
        Ok(KnObject {
            schema: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            comment: r.get(3)?,
            row_estimate: r.get(4)?,
            columns: r.get::<_, Option<String>>(5)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
            foreign_keys: r.get::<_, Option<String>>(6)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        })
    }

    const AI_COLUMNS: &str = "schema_name, name, kind, comment, row_estimate, columns_json, fks_json";

    fn ai_objects_tx(c: &rusqlite::Connection, connection_id: &str, schema: Option<&str>) -> Result<Vec<KnObject>> {
        let mut st = c.prepare_cached(&format!(
            "SELECT {} FROM ai_objects WHERE connection_id = ?1 AND (?2 IS NULL OR schema_name = ?2) ORDER BY schema_name, name",
            Self::AI_COLUMNS
        ))?;
        let rows = st.query_map(params![connection_id, schema], Self::ai_object_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Once, after the v20 migration: what each connection's AI may use.
    pub(crate) fn build_ai_tables(&self) -> Result<()> {
        const DONE: &str = "ai_tables_v20";
        let mut c = self.conn.lock();
        let done: bool = c.query_row("SELECT count(*) > 0 FROM maintenance WHERE name = ?1", [DONE], |r| r.get(0))?;
        if done {
            return Ok(());
        }
        let tx = c.transaction()?;
        let ids: Vec<String> = {
            let mut st = tx.prepare("SELECT DISTINCT connection_id FROM ai_scope")?;
            st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            Self::ai_refresh_tx(&tx, &id, None)?;
        }
        tx.execute("INSERT OR IGNORE INTO maintenance (name, done_at) VALUES (?1, ?2)", params![DONE, crate::now_ms()])?;
        tx.commit()?;
        Ok(())
    }

    /// The tables the AI may use on this connection, and only those.
    pub fn ai_catalog<'a>(&'a self, connection_id: &'a str) -> AiCatalog<'a> {
        AiCatalog { ws: self, connection_id }
    }
}

/// The only reader of table facts for AI code: everything it returns is a
/// table in the connection's AI scope (see the module docs).
#[derive(Clone, Copy)]
pub struct AiCatalog<'a> {
    ws: &'a Workspace,
    connection_id: &'a str,
}

impl AiCatalog<'_> {
    pub fn connection_id(&self) -> &str {
        self.connection_id
    }

    /// A table by `schema.name` or bare `name` (case-insensitive; the schema
    /// may be the last part of a `catalog.schema` id).
    pub fn get(&self, reference: &str) -> Result<Option<KnObject>> {
        // The last name is the table (a quoted one may contain dots).
        let (schema, name) = crate::paths::split_reference(reference);
        let c = self.ws.conn.lock();
        Ok(c.query_row(
            &format!(
                "SELECT {} FROM ai_objects WHERE connection_id = ?1 AND lower(name) = lower(?2) \
                   AND (?3 IS NULL OR lower(schema_name) = lower(?3) OR lower(schema_name) LIKE '%.' || lower(?3)) \
                 ORDER BY length(schema_name) LIMIT 1",
                Workspace::AI_COLUMNS
            ),
            params![self.connection_id, name, schema],
            Workspace::ai_object_row,
        )
        .optional()?)
    }

    /// A target path (`schema.table`, `schema.table.column`, bare `table`):
    /// the table and the column, if any.
    pub fn resolve_path(&self, path: &str) -> Result<Option<(KnObject, Option<String>)>> {
        if let Some(o) = self.get(path)? {
            return Ok(Some((o, None)));
        }
        let mut parts = crate::paths::path_parts(path);
        let Some(col) = parts.pop().filter(|_| !parts.is_empty()) else { return Ok(None) };
        if let Some(o) = self.get(&crate::paths::path_text(&parts))?
            && let Some(c) = o.columns.iter().find(|c| c.name.eq_ignore_ascii_case(&col)).map(|c| c.name.clone())
        {
            return Ok(Some((o, Some(c))));
        }
        Ok(None)
    }

    /// Every table the AI may use, by schema and name.
    pub fn objects(&self) -> Result<Vec<KnObject>> {
        Workspace::ai_objects_tx(&self.ws.conn.lock(), self.connection_id, None)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self.ws.conn.lock().query_row("SELECT count(*) FROM ai_tables WHERE connection_id = ?1", [self.connection_id], |r| r.get(0))?)
    }

    /// A page of the tables whose `schema.name comment` contains `filter`
    /// (case-insensitive; empty = all), and how many match in total.
    pub fn page(&self, filter: &str, offset: usize, limit: usize) -> Result<(Vec<KnObject>, usize)> {
        let c = self.ws.conn.lock();
        let q = filter.trim().to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        const MATCH: &str =
            "connection_id = ?1 AND (?2 = '' OR lower(schema_name || '.' || name || ' ' || coalesce(comment, '')) LIKE '%' || ?2 || '%' ESCAPE '\\')";
        let total: i64 = c.query_row(&format!("SELECT count(*) FROM ai_objects WHERE {MATCH}"), params![self.connection_id, q], |r| r.get(0))?;
        let mut st =
            c.prepare_cached(&format!("SELECT {} FROM ai_objects WHERE {MATCH} ORDER BY schema_name, name LIMIT ?3 OFFSET ?4", Workspace::AI_COLUMNS))?;
        let rows =
            st.query_map(params![self.connection_id, q, limit as i64, offset as i64], Workspace::ai_object_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, total as usize))
    }

    /// Full-text search over the allowed tables and the approved notes.
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<KnHit>> {
        let Some(q) = fts_query(text) else { return Ok(vec![]) };
        let c = self.ws.conn.lock();
        let mut st = c.prepare_cached(
            "SELECT source, ref, title, bm25(kn_fts, 0, 0, 0, 5.0, 1.0) AS score FROM kn_fts \
             WHERE kn_fts MATCH ?2 AND connection_id = ?1 ORDER BY score LIMIT ?3",
        )?;
        let rows = st
            .query_map(params![self.connection_id, q, limit as i64], |r| {
                Ok(KnHit { source: r.get(0)?, reference: r.get(1)?, title: r.get(2)?, score: r.get(3)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Column names of an allowed table (exact schema id, any-case name).
    pub fn column_names(&self, schema: &str, name: &str) -> Result<Option<Vec<String>>> {
        let c = self.ws.conn.lock();
        let json: Option<Option<String>> = c
            .query_row(
                "SELECT columns_json FROM ai_objects WHERE connection_id = ?1 AND schema_name = ?2 AND lower(name) = lower(?3)",
                params![self.connection_id, schema, name],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.map(|j| {
            j.and_then(|j| serde_json::from_str::<Vec<databrain_connector_core::ColumnInfo>>(&j).ok()).unwrap_or_default().into_iter().map(|c| c.name).collect()
        }))
    }

    /// Foreign keys of allowed tables that reference `table`, as (schema,
    /// table, key). `None`: no table is allowed, so it can't tell.
    pub fn foreign_keys_to(&self, table: &str) -> Result<Option<Vec<(String, String, ForeignKey)>>> {
        if self.count()? == 0 {
            return Ok(None);
        }
        let c = self.ws.conn.lock();
        let mut st = c.prepare_cached("SELECT schema_name, name, fks_json FROM ai_objects WHERE connection_id = ?1 AND fks_json LIKE ?2")?;
        let like = format!("%{}%", table.replace('%', "").replace('_', "%"));
        let mut out = Vec::new();
        for row in st.query_map(params![self.connection_id, like], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
            let (schema, name, json) = row?;
            for fk in serde_json::from_str::<Vec<ForeignKey>>(&json).unwrap_or_default() {
                if fk.ref_table.eq_ignore_ascii_case(table) {
                    out.push((schema.clone(), name.clone(), fk));
                }
            }
        }
        Ok(Some(out))
    }

    /// Allowed tables in `schemas`.
    pub fn count_in(&self, schemas: &[String]) -> Result<usize> {
        let n: i64 = self.ws.conn.lock().query_row(
            "SELECT count(*) FROM ai_tables WHERE connection_id = ?1 AND schema_name IN (SELECT value FROM json_each(?2))",
            params![self.connection_id, serde_json::to_string(schemas)?],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::{ColumnInfo, DbObject, ObjectKind};

    fn rule(schema: &str, table: &str, allow: bool) -> ScopeRule {
        ScopeRule { schema: schema.into(), table: table.into(), allow }
    }

    #[test]
    fn the_most_specific_rule_decides() {
        let r = |rules: &[ScopeRule], s: &str, t: &str| scope_allows(rules, s, t);
        assert!(!r(&[], "s", "t"), "no rule: nothing");
        assert!(r(&[rule("*", "", true)], "main.sales", "t"));
        // A schema id beats `catalog.*`, which beats `*`.
        let rules = [rule("*", "", true), rule("main.*", "", false), rule("main.sales", "", true)];
        assert!(r(&rules, "main.sales", "orders"));
        assert!(!r(&rules, "main.hr", "staff"));
        assert!(r(&rules, "dev.x", "t"));
        // A table rule beats its schema's: excluded in an allowed schema,
        // allowed alone in a schema that is off.
        let rules = [rule("sales", "", true), rule("sales", "salaries", false), rule("hr", "teams", true)];
        assert!(r(&rules, "sales", "orders"));
        assert!(!r(&rules, "SALES", "Salaries"), "names are case-insensitive");
        assert!(r(&rules, "hr", "teams"));
        assert!(!r(&rules, "hr", "staff"));
        // At one level, an exclusion wins.
        assert!(!r(&[rule("sales", "", true), rule("SALES", "", false)], "sales", "t"));
        // `catalog.*` needs the dot: `main.*` doesn't cover `mainframe.x`.
        assert!(!r(&[rule("main.*", "", true)], "mainframe.x", "t"));
        // A table rule needs its schema exactly.
        assert!(!r(&[rule("*", "orders", true)], "sales", "orders"));
    }

    fn o(schema: &str, name: &str, kind: ObjectKind) -> DbObject {
        DbObject { schema: schema.into(), name: name.into(), kind, comment: None, row_estimate: None }
    }

    fn col(n: &str) -> ColumnInfo {
        ColumnInfo { name: n.into(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None }
    }

    #[test]
    fn the_ai_sees_only_allowed_tables_and_follows_every_write() {
        let ws = Workspace::open_in_memory().unwrap();
        let ai = ws.ai_catalog("c");
        let names = || ai.objects().unwrap().into_iter().map(|o| o.full_name()).collect::<Vec<_>>();
        ws.meta_put_schema("c", "sales", &[o("sales", "orders", ObjectKind::Table), o("sales", "f", ObjectKind::Function)]).unwrap();
        ws.meta_put_schema("c", "hr", &[o("hr", "salaries", ObjectKind::Table)]).unwrap();
        assert!(names().is_empty(), "nothing until the user allows it");
        assert!(ai.get("sales.orders").unwrap().is_none() && ai.search("orders", 5).unwrap().is_empty());

        ws.ai_set_scope("c", &[rule("sales", "", true)]).unwrap();
        assert_eq!(names(), vec!["sales.orders"], "routines aren't tables");
        assert!(ai.get("hr.salaries").unwrap().is_none(), "out of scope reads as missing");
        assert_eq!(ai.search("orders", 5).unwrap()[0].reference, "sales.orders");

        // A listing adds a table: allowed with its schema.
        ws.meta_put_schema("c", "sales", &[o("sales", "orders", ObjectKind::Table), o("sales", "refunds", ObjectKind::View)]).unwrap();
        assert_eq!(names(), vec!["sales.orders", "sales.refunds"]);
        // A describe: its columns are searchable.
        ws.meta_put_table("c", "sales", "refunds", &[col("refund_amount")], Some(&[])).unwrap();
        assert_eq!(ai.search("refund_amount", 5).unwrap()[0].reference, "sales.refunds");
        assert_eq!(ai.column_names("sales", "REFUNDS").unwrap().unwrap(), vec!["refund_amount"]);
        assert!(ai.column_names("hr", "salaries").unwrap().is_none());
        // A search hit in another schema: not allowed.
        ws.meta_add_objects("c", &[o("hr", "teams", ObjectKind::Table)]).unwrap();
        assert_eq!(ai.count().unwrap(), 2);
        // Allowing one table alone in a schema that is off.
        ws.ai_put_rule("c", &rule("hr", "teams", true)).unwrap();
        assert_eq!(names(), vec!["hr.teams", "sales.orders", "sales.refunds"]);
        // An incremental refresh drops one.
        ws.meta_apply_changes("c", "sales", &[], &["view:refunds".into()], &Default::default(), None).unwrap();
        assert_eq!(names(), vec!["hr.teams", "sales.orders"]);
        assert!(ai.search("refund_amount", 5).unwrap().is_empty(), "its search row went with it");
        // The schema is gone from the server.
        ws.meta_drop_schema("c", "hr").unwrap();
        assert_eq!(names(), vec!["sales.orders"]);
        // Excluding a table in an allowed schema.
        ws.ai_put_rule("c", &rule("sales", "orders", false)).unwrap();
        assert!(names().is_empty() && ai.search("orders", 5).unwrap().is_empty());
        ws.ai_put_rule("c", &rule("sales", "orders", true)).unwrap();
        // The connection points elsewhere: the facts go, the scope stays.
        ws.meta_clear("c").unwrap();
        assert!(names().is_empty() && ai.search("orders", 5).unwrap().is_empty());
        assert_eq!(ws.ai_scope("c").unwrap().len(), 3, "sales, hr.teams, sales.orders");
        ws.meta_put_schema("c", "sales", &[o("sales", "orders", ObjectKind::Table)]).unwrap();
        assert_eq!(names(), vec!["sales.orders"], "listed again: allowed again");
        // Other connections see nothing of it.
        assert!(ws.ai_catalog("other").objects().unwrap().is_empty());
    }

    /// Broader exclusions hold: choosing the schemas to index keeps them,
    /// and indexing a schema a `catalog.*` exclusion covers adds no allow.
    #[test]
    fn index_runs_keep_broad_exclusions() {
        let ws = Workspace::open_in_memory().unwrap();
        ws.ai_put_rule("c", &rule("hr", "", false)).unwrap();
        ws.ai_set_schema_rules("c", &["sales".to_string()]).unwrap();
        assert!(ws.ai_excluded("c", "hr", "t").unwrap(), "hr stays excluded");
        assert!(!ws.ai_excluded("c", "sales", "t").unwrap());
        ws.ai_put_rule("c", &rule("main.*", "", false)).unwrap();
        {
            let mut c = ws.conn.lock();
            let tx = c.transaction().unwrap();
            Workspace::put_rule_if_absent(&tx, "c", &rule("main.hr", "", true)).unwrap();
            Workspace::put_rule_if_absent(&tx, "c", &rule("dev.x", "", true)).unwrap();
            tx.commit().unwrap();
        }
        assert!(ws.ai_excluded("c", "main.hr", "salaries").unwrap(), "main.* still decides");
        assert!(!ws.ai_scope("c").unwrap().iter().any(|r| r.schema == "main.hr"));
        assert!(ws.ai_scope("c").unwrap().iter().any(|r| r.schema == "dev.x" && r.allow), "nothing excludes dev.x");
    }

    #[test]
    fn indexing_a_schema_allows_it_and_dropping_it_from_the_index_doesnt() {
        let ws = Workspace::open_in_memory().unwrap();
        let k = |schema: &str, name: &str| KnObject {
            schema: schema.into(),
            name: name.into(),
            kind: "table".into(),
            comment: None,
            row_estimate: None,
            columns: vec![col("id")],
            foreign_keys: vec![],
        };
        ws.kn_replace_schema("c", "sales", &[k("sales", "orders"), k("sales", "items")]).unwrap();
        ws.kn_upsert_objects("c", &[k("hr", "teams")]).unwrap();
        let ai = ws.ai_catalog("c");
        assert_eq!(ai.count().unwrap(), 3);
        assert_eq!(ws.meta_table("c", "sales", "orders").unwrap().unwrap().columns.len(), 1, "facts in the explorer cache");
        // A re-read without `items`: gone from the server, and from the AI.
        ws.kn_replace_schema("c", "sales", &[k("sales", "orders")]).unwrap();
        assert!(ai.get("sales.items").unwrap().is_none());
        // Out of the index scope: out of the AI's.
        ws.kn_retain_schemas("c", &["hr".into()]).unwrap();
        assert_eq!(ai.objects().unwrap().iter().map(|o| o.full_name()).collect::<Vec<_>>(), vec!["hr.teams"]);
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use databrain_connector_core::{ColumnInfo, DbObject, ObjectKind};

    fn k(schema: &str, name: &str) -> KnObject {
        KnObject {
            schema: schema.into(),
            name: name.into(),
            kind: "table".into(),
            comment: None,
            row_estimate: None,
            columns: vec![ColumnInfo { name: "id".into(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None }],
            foreign_keys: vec![],
        }
    }

    #[test]
    fn an_exclusion_survives_indexing_and_scope_changes() {
        let ws = Workspace::open_in_memory().unwrap();
        ws.kn_replace_schema("c", "sales", &[k("sales", "orders"), k("sales", "salaries")]).unwrap();
        ws.ai_put_rule("c", &ScopeRule { schema: "sales".into(), table: "salaries".into(), allow: false }).unwrap();
        let ai = ws.ai_catalog("c");
        let names = || ai.objects().unwrap().into_iter().map(|o| o.full_name()).collect::<Vec<_>>();
        assert_eq!(names(), vec!["sales.orders"]);
        assert!(ws.ai_excluded("c", "sales", "salaries").unwrap());
        assert!(!ws.ai_excluded("c", "hr", "teams").unwrap(), "no rule is not an exclusion");
        // index_tables (the AI asked, approved), a re-index: the exclusion stays.
        ws.kn_upsert_objects("c", &[k("sales", "salaries")]).unwrap();
        ws.kn_replace_schema("c", "sales", &[k("sales", "orders"), k("sales", "salaries")]).unwrap();
        assert_eq!(names(), vec!["sales.orders"]);
        // A table allowed on its own elsewhere, then the schema leaves the index scope:
        // only its schema-level rule goes.
        ws.kn_upsert_objects("c", &[k("hr", "teams")]).unwrap();
        ws.kn_retain_schemas("c", &["hr".into()]).unwrap();
        assert!(ws.ai_scope("c").unwrap().contains(&ScopeRule { schema: "sales".into(), table: "salaries".into(), allow: false }));
        assert!(ws.ai_scope("c").unwrap().contains(&ScopeRule { schema: "hr".into(), table: "teams".into(), allow: true }));
        assert_eq!(names(), vec!["hr.teams"]);
        // A `catalog.*`-like broad rule later doesn't bring the excluded one back.
        ws.ai_put_rule("c", &ScopeRule { schema: "*".into(), table: String::new(), allow: true }).unwrap();
        ws.meta_put_schema(
            "c",
            "sales",
            &[DbObject { schema: "sales".into(), name: "salaries".into(), kind: ObjectKind::Table, comment: None, row_estimate: None }],
        )
        .unwrap();
        assert!(!names().contains(&"sales.salaries".to_string()));
    }

    #[test]
    fn search_rows_follow_each_schema_even_when_names_share_a_prefix() {
        let ws = Workspace::open_in_memory().unwrap();
        let o = |s: &str, n: &str| DbObject { schema: s.into(), name: n.into(), kind: ObjectKind::Table, comment: None, row_estimate: None };
        ws.ai_set_scope("c", &[ScopeRule { schema: "*".into(), table: String::new(), allow: true }]).unwrap();
        ws.meta_put_schema("c", "main.sales", &[o("main.sales", "invoices")]).unwrap();
        ws.meta_put_schema("c", "main", &[o("main", "orders")]).unwrap();
        // Listing `main` again leaves `main.sales` alone.
        ws.meta_put_schema("c", "main", &[o("main", "orders")]).unwrap();
        let ai = ws.ai_catalog("c");
        assert_eq!(ai.search("invoices", 5).unwrap().len(), 1);
        assert_eq!(ai.search("orders", 5).unwrap().len(), 1, "one row, not two");
        // Unchanged tables keep their search row (no rewrite); gone ones lose it.
        ws.meta_put_schema("c", "main", &[]).unwrap();
        assert!(ai.search("orders", 5).unwrap().is_empty());
        assert_eq!(ai.search("invoices", 5).unwrap().len(), 1);
        // A connection with no scope does nothing (and has nothing).
        ws.meta_put_schema("other", "main", &[o("main", "orders")]).unwrap();
        assert!(ws.ai_catalog("other").search("orders", 5).unwrap().is_empty());
    }
}
