//! Knowledge base storage: indexed database objects, notes/glossary and a
//! full-text index (SQLite FTS5, BM25 ranking) for retrieval.

use std::collections::HashSet;

use databrain_connector_core::{ColumnInfo, ForeignKey};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Workspace, new_id, now_ms};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnObject {
    pub schema: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub row_estimate: Option<i64>,
    pub columns: Vec<ColumnInfo>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
}

impl KnObject {
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    fn hash(&self) -> String {
        // FNV-1a over the serialized object: cheap change detection.
        let s = serde_json::to_string(self).unwrap_or_default();
        let mut h: u64 = 0xcbf29ce484222325;
        for b in s.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    }

    /// Text indexed for search: names (also split on `_`), comments, columns.
    fn search_body(&self) -> String {
        let mut out = String::new();
        let push_ident = |out: &mut String, s: &str| {
            out.push_str(s);
            out.push(' ');
            if s.contains('_') {
                out.push_str(&s.replace('_', " "));
                out.push(' ');
            }
        };
        push_ident(&mut out, &self.name);
        out.push_str(&self.schema);
        out.push(' ');
        out.push_str(&self.kind);
        out.push(' ');
        if let Some(c) = &self.comment {
            out.push_str(c);
            out.push(' ');
        }
        for c in &self.columns {
            push_ident(&mut out, &c.name);
            out.push_str(&c.data_type);
            out.push(' ');
            if let Some(cm) = &c.comment {
                out.push_str(cm);
                out.push(' ');
            }
        }
        for fk in &self.foreign_keys {
            out.push_str("references ");
            push_ident(&mut out, &fk.ref_table);
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteStatus {
    Approved,
    /// Suggested by the AI; excluded from retrieval until approved.
    Proposed,
}

impl NoteStatus {
    fn as_str(self) -> &'static str {
        match self {
            NoteStatus::Approved => "approved",
            NoteStatus::Proposed => "proposed",
        }
    }
    fn parse(s: &str) -> Self {
        if s == "proposed" { NoteStatus::Proposed } else { NoteStatus::Approved }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnNote {
    #[serde(default)]
    pub id: String,
    pub connection_id: String,
    /// `schema.table`, `schema.table.column`, or `None` for glossary entries.
    #[serde(default)]
    pub target: Option<String>,
    pub body: String,
    /// `user` or `ai`.
    #[serde(default = "user")]
    pub author: String,
    #[serde(default = "approved")]
    pub status: NoteStatus,
    #[serde(default)]
    pub created_at: i64,
}

fn user() -> String {
    "user".into()
}
fn approved() -> NoteStatus {
    NoteStatus::Approved
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnState {
    pub connection_id: String,
    pub indexed_at: i64,
    pub objects: i64,
    pub schemas: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KnHit {
    /// `object` or `note`.
    pub source: String,
    /// `schema.name` for objects, note id for notes.
    pub reference: String,
    pub title: String,
    /// BM25 score (lower is better).
    pub score: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct IndexDelta {
    pub changed: usize,
    pub removed: usize,
    pub unchanged: usize,
}

/// Build a safe FTS5 query: every token as a quoted prefix term, OR-ed.
pub fn fts_query(text: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let terms: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .flat_map(|w| {
            let mut v = vec![w.to_string()];
            if w.contains('_') {
                v.extend(w.split('_').map(str::to_string));
            }
            v
        })
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() >= 2 && !STOPWORDS.contains(&w.as_str()))
        .filter(|w| seen.insert(w.clone()))
        .take(24)
        .map(|w| {
            // Crude singularization so "orders" also finds "order_items".
            let stem = if w.len() > 4 && w.ends_with('s') && !w.ends_with("ss") { &w[..w.len() - 1] } else { &w[..] };
            format!("\"{}\"*", stem.replace('"', ""))
        })
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "that", "this", "what", "which", "show", "me", "all", "of", "in", "by", "to",
    "is", "are", "was", "how", "many", "each", "per", "a", "an", "on", "at", "give", "list", "get", "find", "select",
    "where", "top", "last", "first", "please", "can", "you", "my", "our", "query", "sql", "write",
];

impl Workspace {
    /// Replace the indexed objects of one schema. Unchanged objects (same
    /// content hash) are skipped.
    pub fn kn_replace_schema(&self, connection_id: &str, schema: &str, objects: &[KnObject]) -> Result<IndexDelta> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let existing: Vec<(String, String)> = {
            let mut stmt = tx.prepare("SELECT name, content_hash FROM kn_objects WHERE connection_id = ?1 AND schema_name = ?2")?;
            stmt.query_map(params![connection_id, schema], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let now = now_ms();
        let mut delta = IndexDelta::default();
        let keep: HashSet<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        for (name, _) in existing.iter().filter(|(n, _)| !keep.contains(n.as_str())) {
            let r = format!("{schema}.{name}");
            tx.execute("DELETE FROM kn_objects WHERE connection_id = ?1 AND schema_name = ?2 AND name = ?3", params![connection_id, schema, name])?;
            tx.execute("DELETE FROM kn_fts WHERE connection_id = ?1 AND source = 'object' AND ref = ?2", params![connection_id, r])?;
            delta.removed += 1;
        }
        for o in objects {
            let hash = o.hash();
            if existing.iter().any(|(n, h)| *n == o.name && *h == hash) {
                delta.unchanged += 1;
                continue;
            }
            let r = o.full_name();
            tx.execute(
                "INSERT INTO kn_objects (connection_id, schema_name, name, kind, comment, row_estimate, columns_json, fks_json, content_hash, indexed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT(connection_id, schema_name, name) DO UPDATE SET kind = excluded.kind, comment = excluded.comment, \
                   row_estimate = excluded.row_estimate, columns_json = excluded.columns_json, fks_json = excluded.fks_json, \
                   content_hash = excluded.content_hash, indexed_at = excluded.indexed_at",
                params![
                    connection_id,
                    o.schema,
                    o.name,
                    o.kind,
                    o.comment,
                    o.row_estimate,
                    serde_json::to_string(&o.columns)?,
                    serde_json::to_string(&o.foreign_keys)?,
                    hash,
                    now
                ],
            )?;
            tx.execute("DELETE FROM kn_fts WHERE connection_id = ?1 AND source = 'object' AND ref = ?2", params![connection_id, r])?;
            tx.execute(
                "INSERT INTO kn_fts (connection_id, source, ref, title, body) VALUES (?1, 'object', ?2, ?3, ?4)",
                params![connection_id, r, o.name, o.search_body()],
            )?;
            delta.changed += 1;
        }
        tx.commit()?;
        Ok(delta)
    }

    /// Remove indexed schemas that no longer exist / are excluded.
    pub fn kn_retain_schemas(&self, connection_id: &str, schemas: &[String]) -> Result<usize> {
        let current: Vec<String> = {
            let c = self.conn.lock();
            let mut stmt = c.prepare("SELECT DISTINCT schema_name FROM kn_objects WHERE connection_id = ?1")?;
            stmt.query_map([connection_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut removed = 0;
        for s in current.iter().filter(|s| !schemas.contains(s)) {
            removed += self.kn_replace_schema(connection_id, s, &[])?.removed;
        }
        Ok(removed)
    }

    pub fn kn_set_state(&self, st: &KnState) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO kn_state (connection_id, indexed_at, objects, schemas_json, error) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(connection_id) DO UPDATE SET indexed_at = excluded.indexed_at, objects = excluded.objects, \
               schemas_json = excluded.schemas_json, error = excluded.error",
            params![st.connection_id, st.indexed_at, st.objects, serde_json::to_string(&st.schemas)?, st.error],
        )?;
        Ok(())
    }

    pub fn kn_state(&self, connection_id: &str) -> Result<Option<KnState>> {
        let c = self.conn.lock();
        Ok(c.query_row(
            "SELECT connection_id, indexed_at, objects, schemas_json, error FROM kn_state WHERE connection_id = ?1",
            [connection_id],
            |r| {
                Ok(KnState {
                    connection_id: r.get(0)?,
                    indexed_at: r.get(1)?,
                    objects: r.get(2)?,
                    schemas: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    error: r.get(4)?,
                })
            },
        )
        .optional()?)
    }

    pub fn kn_count(&self, connection_id: &str) -> Result<i64> {
        Ok(self.conn.lock().query_row("SELECT count(*) FROM kn_objects WHERE connection_id = ?1", [connection_id], |r| r.get(0))?)
    }

    fn kn_object_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<KnObject> {
        Ok(KnObject {
            schema: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            comment: r.get(3)?,
            row_estimate: r.get(4)?,
            columns: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
            foreign_keys: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
        })
    }

    pub fn kn_objects(&self, connection_id: &str) -> Result<Vec<KnObject>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT schema_name, name, kind, comment, row_estimate, columns_json, fks_json FROM kn_objects \
             WHERE connection_id = ?1 ORDER BY schema_name, name",
        )?;
        let rows = stmt.query_map([connection_id], Self::kn_object_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Look up an object by `schema.name` or bare `name` (case-insensitive).
    pub fn kn_get(&self, connection_id: &str, reference: &str) -> Result<Option<KnObject>> {
        let c = self.conn.lock();
        let (schema, name) = match reference.rsplit_once('.') {
            Some((s, n)) => (Some(s.to_string()), n.to_string()),
            None => (None, reference.to_string()),
        };
        let name = name.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']').to_string();
        let schema = schema.map(|s| s.replace(['"', '`', '[', ']'], ""));
        Ok(c.query_row(
            "SELECT schema_name, name, kind, comment, row_estimate, columns_json, fks_json FROM kn_objects \
             WHERE connection_id = ?1 AND lower(name) = lower(?2) \
               AND (?3 IS NULL OR lower(schema_name) = lower(?3) OR lower(schema_name) LIKE '%.' || lower(?3)) \
             ORDER BY length(schema_name) LIMIT 1",
            params![connection_id, name, schema],
            Self::kn_object_row,
        )
        .optional()?)
    }

    pub fn kn_search(&self, connection_id: &str, text: &str, limit: usize) -> Result<Vec<KnHit>> {
        let Some(q) = fts_query(text) else { return Ok(vec![]) };
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT source, ref, title, bm25(kn_fts, 0, 0, 0, 5.0, 1.0) AS score FROM kn_fts \
             WHERE kn_fts MATCH ?2 AND connection_id = ?1 ORDER BY score LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![connection_id, q, limit as i64], |r| {
                Ok(KnHit { source: r.get(0)?, reference: r.get(1)?, title: r.get(2)?, score: r.get(3)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn kn_clear(&self, connection_id: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM kn_objects WHERE connection_id = ?1", [connection_id])?;
        c.execute("DELETE FROM kn_fts WHERE connection_id = ?1 AND source = 'object'", [connection_id])?;
        c.execute("DELETE FROM kn_state WHERE connection_id = ?1", [connection_id])?;
        Ok(())
    }

    pub fn kn_save_note(&self, mut n: KnNote) -> Result<KnNote> {
        if n.body.trim().is_empty() {
            return Err(Error::Invalid("note text is required".into()));
        }
        if n.id.is_empty() {
            n.id = new_id();
            n.created_at = now_ms();
        }
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO kn_notes (id, connection_id, target, body, author, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(id) DO UPDATE SET target = excluded.target, body = excluded.body, status = excluded.status",
            params![n.id, n.connection_id, n.target, n.body.trim(), n.author, n.status.as_str(), n.created_at],
        )?;
        c.execute("DELETE FROM kn_fts WHERE source = 'note' AND ref = ?1", [&n.id])?;
        if n.status == NoteStatus::Approved {
            c.execute(
                "INSERT INTO kn_fts (connection_id, source, ref, title, body) VALUES (?1, 'note', ?2, ?3, ?4)",
                params![n.connection_id, n.id, n.target.clone().unwrap_or_else(|| "glossary".into()), format!("{} {}", n.target.clone().unwrap_or_default().replace(['_', '.'], " "), n.body)],
            )?;
        }
        Ok(n)
    }

    pub fn kn_notes(&self, connection_id: &str) -> Result<Vec<KnNote>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, connection_id, target, body, author, status, created_at FROM kn_notes WHERE connection_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([connection_id], |r| {
                Ok(KnNote {
                    id: r.get(0)?,
                    connection_id: r.get(1)?,
                    target: r.get(2)?,
                    body: r.get(3)?,
                    author: r.get(4)?,
                    status: NoteStatus::parse(&r.get::<_, String>(5)?),
                    created_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn kn_delete_note(&self, id: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM kn_notes WHERE id = ?1", [id])?;
        c.execute("DELETE FROM kn_fts WHERE source = 'note' AND ref = ?1", [id])?;
        Ok(())
    }

    /// Saved queries flagged as AI examples for a connection.
    pub fn ai_examples(&self, connection_id: &str) -> Result<Vec<crate::SavedQuery>> {
        Ok(self
            .list_saved_queries(None)?
            .into_iter()
            .filter(|q| q.ai_example && q.connection_id.as_deref().is_none_or(|c| c == connection_id))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(schema: &str, name: &str, cols: &[&str], comment: Option<&str>) -> KnObject {
        KnObject {
            schema: schema.into(),
            name: name.into(),
            kind: "table".into(),
            comment: comment.map(str::to_string),
            row_estimate: None,
            columns: cols
                .iter()
                .map(|c| ColumnInfo { name: c.to_string(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None })
                .collect(),
            foreign_keys: vec![],
        }
    }

    #[test]
    fn index_search_incremental() {
        let ws = Workspace::open_in_memory().unwrap();
        let objs = vec![
            obj("public", "customer_orders", &["order_id", "customer_id", "total_amount"], Some("All web orders")),
            obj("public", "customers", &["customer_id", "email"], None),
            obj("public", "inventory", &["sku", "qty"], None),
        ];
        let d = ws.kn_replace_schema("c1", "public", &objs).unwrap();
        assert_eq!(d.changed, 3);
        let d = ws.kn_replace_schema("c1", "public", &objs[..2]).unwrap();
        assert_eq!((d.changed, d.removed, d.unchanged), (0, 1, 2));

        let hits = ws.kn_search("c1", "revenue by customer orders", 5).unwrap();
        assert_eq!(hits[0].reference, "public.customer_orders");
        assert!(ws.kn_search("c1", "", 5).unwrap().is_empty());
        assert!(ws.kn_search("other", "orders", 5).unwrap().is_empty());
        // FTS syntax characters are neutralized.
        assert!(ws.kn_search("c1", "\"orders\" AND (x OR", 5).is_ok());

        assert_eq!(ws.kn_get("c1", "CUSTOMERS").unwrap().unwrap().columns.len(), 2);
        assert!(ws.kn_get("c1", "public.customers").unwrap().is_some());
        assert!(ws.kn_get("c1", "nope").unwrap().is_none());

        ws.kn_retain_schemas("c1", &[]).unwrap();
        assert_eq!(ws.kn_count("c1").unwrap(), 0);
    }

    #[test]
    fn notes_only_indexed_when_approved() {
        let ws = Workspace::open_in_memory().unwrap();
        let n = ws
            .kn_save_note(KnNote {
                id: String::new(),
                connection_id: "c1".into(),
                target: Some("public.orders".into()),
                body: "MRR means monthly recurring revenue from subscriptions".into(),
                author: "ai".into(),
                status: NoteStatus::Proposed,
                created_at: 0,
            })
            .unwrap();
        assert!(ws.kn_search("c1", "recurring revenue", 5).unwrap().is_empty());
        let mut a = n.clone();
        a.status = NoteStatus::Approved;
        ws.kn_save_note(a).unwrap();
        let hits = ws.kn_search("c1", "recurring revenue", 5).unwrap();
        assert_eq!(hits[0].source, "note");
        ws.kn_delete_note(&n.id).unwrap();
        assert!(ws.kn_notes("c1").unwrap().is_empty());
    }

    #[test]
    fn fts_query_is_safe() {
        assert_eq!(fts_query("Show me orders!").unwrap(), "\"order\"*");
        assert_eq!(fts_query("customer_id"), Some("\"customer_id\"* OR \"customer\"* OR \"id\"*".into()));
        assert_eq!(fts_query("the of"), None);
    }
}
