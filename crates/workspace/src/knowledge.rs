//! Knowledge base storage: indexed database objects, notes/glossary and a
//! full-text index (SQLite FTS5, BM25 ranking) for retrieval.

use std::collections::HashSet;

use databrain_connector_core::{ColumnInfo, ForeignKey};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Workspace, new_id, now_ms};

/// (schema, name, kind, comment) of a completion hit from the index.
pub type KnCompletion = (String, String, String, Option<String>);

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
    pub fn as_str(self) -> &'static str {
        match self {
            NoteStatus::Approved => "approved",
            NoteStatus::Proposed => "proposed",
        }
    }
    fn parse(s: &str) -> Self {
        if s == "proposed" { NoteStatus::Proposed } else { NoteStatus::Approved }
    }
}

/// Separators between tables in a note target: `a & b`, `a and b`, `a or b`, `a, b`.
pub const TARGET_SEPARATORS: [&str; 4] = ["&", ",", "and", "or"];

/// Table paths of a note target with the separator before each one (`""` for
/// the first): `sales.orders & crm.customers.id` → [("", "sales.orders"), ("&", "crm.customers.id")].
/// Identifier quotes are dropped. A target without a separator is one path.
pub fn split_target(target: &str) -> Vec<(String, String)> {
    let spaced = target.replace('&', " & ").replace(',', " , ");
    let mut out: Vec<(String, String)> = Vec::new();
    let mut sep = String::new();
    for tok in spaced.split_whitespace() {
        let low = tok.to_ascii_lowercase();
        if TARGET_SEPARATORS.contains(&low.as_str()) {
            sep = low;
            continue;
        }
        let path: String = tok.chars().filter(|c| !matches!(c, '"' | '`' | '[' | ']')).collect();
        let path = path.trim_matches('.').to_string();
        if path.is_empty() {
            continue;
        }
        // Two paths with no separator between them: treat as "&".
        let s = if out.is_empty() { String::new() } else if sep.is_empty() { "&".into() } else { std::mem::take(&mut sep) };
        sep.clear();
        out.push((s, path));
    }
    out
}

/// Join paths back into a target, e.g. `sales.orders & crm.customers`.
pub fn join_target(parts: &[(String, String)]) -> String {
    let mut s = String::new();
    for (sep, path) in parts {
        if !s.is_empty() {
            match sep.as_str() {
                "," => s.push_str(", "),
                other => {
                    s.push(' ');
                    s.push_str(if other.is_empty() { "&" } else { other });
                    s.push(' ');
                }
            }
        }
        s.push_str(path);
    }
    s
}

/// Does the note target mention the table `full` (`schema.table`), alone, in a
/// list, or through one of its columns?
pub fn target_mentions(target: &str, full: &str, name: &str) -> bool {
    let full = full.to_ascii_lowercase();
    split_target(target).iter().any(|(_, p)| {
        let p = p.to_ascii_lowercase();
        p == full || p.eq_ignore_ascii_case(name) || p.starts_with(&format!("{full}.")) || full.ends_with(&format!(".{p}"))
    })
}

/// `format` value of a notes file.
pub const NOTES_FORMAT: &str = "databrain-notes";

/// Shareable notes & glossary file (`*.databrain-notes.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotesFile {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub exported_at: i64,
    #[serde(default)]
    pub source: Option<NotesSource>,
    pub notes: Vec<NoteEntry>,
}

impl NotesFile {
    pub fn validate(&self) -> Result<()> {
        if self.format != NOTES_FORMAT {
            return Err(Error::Invalid("not a DataBrain notes file".into()));
        }
        if self.version > 1 {
            return Err(Error::Invalid(format!("notes file version {} is newer than this app supports", self.version)));
        }
        if self.notes.len() > 20_000 {
            return Err(Error::Invalid("notes file has too many notes".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotesSource {
    pub connection: String,
    pub kind: String,
}

/// One note in a file (no ids: they are local).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteEntry {
    #[serde(default)]
    pub target: Option<String>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    /// Nothing similar exists.
    New,
    /// Same subject and same text: nothing to do.
    Same,
    /// Same subject (target, or glossary term), different text.
    Conflict,
}

/// One incoming note compared with the existing notes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportItem {
    pub incoming: NoteEntry,
    pub kind: ImportKind,
    /// Existing notes about the same subject.
    pub existing: Vec<KnNote>,
    /// Why the target cannot be used on this connection (tables missing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_target: Option<String>,
}

/// What to do with an incoming note: `add`, `skip`, `replace` (delete
/// `existing_ids`, add the incoming text) or `merge` (delete `existing_ids`,
/// add `body`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportAction {
    pub incoming: NoteEntry,
    pub action: String,
    #[serde(default)]
    pub existing_ids: Vec<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
}

/// Subject of a note: its target, or for glossary notes the term before
/// `:` / `=` / ` - ` (e.g. "active customer = …" → "active customer").
pub fn note_subject(target: Option<&str>, body: &str) -> String {
    if let Some(t) = target.map(str::trim).filter(|t| !t.is_empty()) {
        return format!("t:{}", t.to_lowercase());
    }
    let first = body.trim().lines().next().unwrap_or("");
    let cut = [":", "=", " - ", " — ", " means "].iter().filter_map(|s| first.find(s)).min();
    match cut {
        Some(i) if i > 0 && i <= 60 => format!("g:{}", normalize(&first[..i])),
        _ => format!("b:{}", normalize(body)),
    }
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace()).to_string()
}

/// Compare incoming notes with existing ones (see [`ImportKind`]).
pub fn plan_import(existing: &[KnNote], incoming: &[NoteEntry]) -> Vec<ImportItem> {
    let mut seen = HashSet::new();
    incoming
        .iter()
        .filter(|n| !n.body.trim().is_empty())
        // Duplicates inside the file: keep the first.
        .filter(|n| seen.insert((note_subject(n.target.as_deref(), &n.body), normalize(&n.body))))
        .map(|n| {
            let subject = note_subject(n.target.as_deref(), &n.body);
            let body = normalize(&n.body);
            let same_subject: Vec<KnNote> = existing.iter().filter(|e| note_subject(e.target.as_deref(), &e.body) == subject).cloned().collect();
            let identical = existing.iter().any(|e| normalize(&e.body) == body && e.target.as_deref().map(str::to_lowercase) == n.target.as_deref().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()));
            let kind = if identical {
                ImportKind::Same
            } else if same_subject.is_empty() {
                ImportKind::New
            } else {
                ImportKind::Conflict
            };
            ImportItem { incoming: n.clone(), kind, existing: if kind == ImportKind::Conflict { same_subject } else { vec![] }, invalid_target: None }
        })
        .collect()
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
    /// AI proposal that updates an existing note: approving it replaces that note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
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
        let existing: std::collections::HashMap<String, String> = {
            let mut stmt = tx.prepare("SELECT name, content_hash FROM kn_objects WHERE connection_id = ?1 AND schema_name = ?2")?;
            stmt.query_map(params![connection_id, schema], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let now = now_ms();
        let mut delta = IndexDelta::default();
        let keep: HashSet<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        // Search rows to drop: removed and changed objects. Deleted in one
        // statement at the end: a `ref = ?` delete scans the whole FTS table,
        // so one per object made big schemas quadratic (and held the
        // workspace lock, stalling the app).
        let mut stale_refs: Vec<String> = Vec::new();
        for name in existing.keys().filter(|n| !keep.contains(n.as_str())) {
            tx.execute("DELETE FROM kn_objects WHERE connection_id = ?1 AND schema_name = ?2 AND name = ?3", params![connection_id, schema, name])?;
            stale_refs.push(format!("{schema}.{name}"));
            delta.removed += 1;
        }
        let mut fresh: Vec<&KnObject> = Vec::new();
        for o in objects {
            let hash = o.hash();
            match existing.get(&o.name) {
                Some(h) if *h == hash => {
                    delta.unchanged += 1;
                    continue;
                }
                Some(_) => stale_refs.push(o.full_name()),
                None => {}
            }
            fresh.push(o);
            tx.prepare_cached(
                "INSERT INTO kn_objects (connection_id, schema_name, name, kind, comment, row_estimate, columns_json, fks_json, content_hash, indexed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT(connection_id, schema_name, name) DO UPDATE SET kind = excluded.kind, comment = excluded.comment, \
                   row_estimate = excluded.row_estimate, columns_json = excluded.columns_json, fks_json = excluded.fks_json, \
                   content_hash = excluded.content_hash, indexed_at = excluded.indexed_at",
            )?
            .execute(params![
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
            delta.changed += 1;
        }
        if !stale_refs.is_empty() {
            tx.execute(
                "DELETE FROM kn_fts WHERE rowid IN (SELECT rowid FROM kn_fts WHERE connection_id = ?1 AND source = 'object' AND ref IN (SELECT value FROM json_each(?2)))",
                params![connection_id, serde_json::to_string(&stale_refs)?],
            )?;
        }
        {
            let mut ins = tx.prepare_cached("INSERT INTO kn_fts (connection_id, source, ref, title, body) VALUES (?1, 'object', ?2, ?3, ?4)")?;
            for o in fresh {
                ins.execute(params![connection_id, o.full_name(), o.name, o.search_body()])?;
            }
        }
        tx.commit()?;
        Ok(delta)
    }

    /// Fingerprints of the schemas as last indexed (see `Session::schema_fingerprints`).
    pub fn kn_fingerprints(&self, connection_id: &str) -> Result<std::collections::HashMap<String, String>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare("SELECT schema_name, fingerprint FROM kn_schema_state WHERE connection_id = ?1")?;
        let rows = stmt.query_map([connection_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Record (or forget, with `None`) the fingerprint a schema was indexed at.
    pub fn kn_set_fingerprint(&self, connection_id: &str, schema: &str, fingerprint: Option<&str>) -> Result<()> {
        let c = self.conn.lock();
        match fingerprint {
            Some(f) => c.execute(
                "INSERT INTO kn_schema_state (connection_id, schema_name, fingerprint, indexed_at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(connection_id, schema_name) DO UPDATE SET fingerprint = excluded.fingerprint, indexed_at = excluded.indexed_at",
                params![connection_id, schema, f, now_ms()],
            )?,
            None => c.execute("DELETE FROM kn_schema_state WHERE connection_id = ?1 AND schema_name = ?2", params![connection_id, schema])?,
        };
        Ok(())
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
        self.conn.lock().execute(
            "DELETE FROM kn_schema_state WHERE connection_id = ?1 AND schema_name NOT IN (SELECT value FROM json_each(?2))",
            params![connection_id, serde_json::to_string(schemas)?],
        )?;
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

    /// Number of indexed objects in the given schemas.
    pub fn kn_objects_in(&self, connection_id: &str, schemas: &[String]) -> Result<usize> {
        let n: i64 = self.conn.lock().query_row(
            "SELECT count(*) FROM kn_objects WHERE connection_id = ?1 AND schema_name IN (SELECT value FROM json_each(?2))",
            params![connection_id, serde_json::to_string(schemas)?],
            |r| r.get(0),
        )?;
        Ok(n as usize)
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

    /// A page of indexed objects whose `schema.name comment` contains `filter`
    /// (case-insensitive; empty = all), ordered by schema and name, plus how
    /// many match in total. Keeps the Knowledge view fast on 10k+ tables.
    pub fn kn_list_objects(&self, connection_id: &str, filter: &str, offset: usize, limit: usize) -> Result<(Vec<KnObject>, usize)> {
        let c = self.conn.lock();
        let q = filter.trim().to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        const MATCH: &str = "connection_id = ?1 AND (?2 = '' OR lower(schema_name || '.' || name || ' ' || coalesce(comment, '')) LIKE '%' || ?2 || '%' ESCAPE '\\')";
        let total: i64 = c.query_row(&format!("SELECT count(*) FROM kn_objects WHERE {MATCH}"), params![connection_id, q], |r| r.get(0))?;
        let mut stmt = c.prepare_cached(&format!(
            "SELECT schema_name, name, kind, comment, row_estimate, columns_json, fks_json FROM kn_objects WHERE {MATCH} \
             ORDER BY schema_name, name LIMIT ?3 OFFSET ?4"
        ))?;
        let rows = stmt.query_map(params![connection_id, q, limit as i64, offset as i64], Self::kn_object_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, total as usize))
    }

    /// Completion from the local index (no network): tables/views whose name
    /// contains `query` (empty = any), in `schema` when given, prefix matches
    /// and short names first. Returns (schema, name, kind, comment).
    pub fn kn_complete(&self, connection_id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<KnCompletion>> {
        let c = self.conn.lock();
        let q = query.trim().to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let mut stmt = c.prepare_cached(
            // The knowledge index and the metadata cache (tables seen while browsing).
            "SELECT schema_name, name, max(kind), max(comment) FROM ( \
                 SELECT schema_name, name, kind, comment FROM kn_objects WHERE connection_id = ?1 \
                 UNION ALL SELECT schema_name, name, kind, comment FROM meta_objects WHERE connection_id = ?1 \
             ) WHERE (?2 IS NULL OR schema_name = ?2) AND lower(name) LIKE '%' || ?3 || '%' ESCAPE '\\' \
             GROUP BY schema_name, name \
             ORDER BY lower(name) NOT LIKE ?3 || '%' ESCAPE '\\', length(name), name LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(params![connection_id, schema, q, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Column names of a table (exact schema id): knowledge index, else the metadata cache.
    pub fn kn_column_names(&self, connection_id: &str, schema: &str, name: &str) -> Result<Option<Vec<String>>> {
        if let Some(cols) = self.kn_indexed_column_names(connection_id, schema, name)? {
            return Ok(Some(cols));
        }
        self.meta_column_names(connection_id, schema, name)
    }

    fn kn_indexed_column_names(&self, connection_id: &str, schema: &str, name: &str) -> Result<Option<Vec<String>>> {
        let c = self.conn.lock();
        let json: Option<String> = c
            .query_row(
                "SELECT columns_json FROM kn_objects WHERE connection_id = ?1 AND schema_name = ?2 AND lower(name) = lower(?3)",
                params![connection_id, schema, name],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.map(|j| serde_json::from_str::<Vec<ColumnInfo>>(&j).unwrap_or_default().into_iter().map(|c| c.name).collect()))
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

    /// Resolve one target path (`schema.table` or `schema.table.column`, bare
    /// `table` too) against the index: the table and the column, if any.
    pub fn kn_resolve_path(&self, connection_id: &str, path: &str) -> Result<Option<(KnObject, Option<String>)>> {
        if let Some(o) = self.kn_get(connection_id, path)? {
            return Ok(Some((o, None)));
        }
        let Some((table, col)) = path.rsplit_once('.') else { return Ok(None) };
        if let Some(o) = self.kn_get(connection_id, table)? {
            if let Some(c) = o.columns.iter().find(|c| c.name.eq_ignore_ascii_case(col)).map(|c| c.name.clone()) {
                return Ok(Some((o, Some(c))));
            }
        }
        Ok(None)
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
        c.execute("DELETE FROM kn_schema_state WHERE connection_id = ?1", [connection_id])?;
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
        n.target = n.target.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let c = self.conn.lock();
        // Approving an update: the new text takes the old note's place.
        if n.status == NoteStatus::Approved {
            if let Some(old) = n.replaces.take() {
                if old != n.id {
                    c.execute("DELETE FROM kn_notes WHERE id = ?1 AND connection_id = ?2", params![old, n.connection_id])?;
                    c.execute("DELETE FROM kn_fts WHERE source = 'note' AND ref = ?1", [&old])?;
                }
            }
        }
        c.execute(
            "INSERT INTO kn_notes (id, connection_id, target, body, author, status, created_at, replaces) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(id) DO UPDATE SET target = excluded.target, body = excluded.body, status = excluded.status, replaces = excluded.replaces",
            params![n.id, n.connection_id, n.target, n.body.trim(), n.author, n.status.as_str(), n.created_at, n.replaces],
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
            "SELECT id, connection_id, target, body, author, status, created_at, replaces FROM kn_notes WHERE connection_id = ?1 ORDER BY created_at DESC",
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
                    replaces: r.get(7)?,
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

    /// Approved notes of a connection as a shareable file.
    pub fn kn_export_notes(&self, connection_id: &str) -> Result<NotesFile> {
        let profile = self.get_connection(connection_id)?;
        let mut notes: Vec<NoteEntry> = self
            .kn_notes(connection_id)?
            .into_iter()
            .filter(|n| n.status == NoteStatus::Approved)
            .map(|n| NoteEntry { target: n.target, body: n.body, author: Some(n.author) })
            .collect();
        notes.sort_by(|a, b| (a.target.is_none(), a.target.as_deref().unwrap_or(""), &a.body).cmp(&(b.target.is_none(), b.target.as_deref().unwrap_or(""), &b.body)));
        Ok(NotesFile {
            format: NOTES_FORMAT.into(),
            version: 1,
            exported_at: now_ms(),
            source: Some(NotesSource { connection: profile.name, kind: profile.config.kind.as_str().to_string() }),
            notes,
        })
    }

    /// Compare a notes file with the connection's approved notes.
    pub fn kn_import_plan(&self, connection_id: &str, file: &NotesFile) -> Result<Vec<ImportItem>> {
        file.validate()?;
        let existing: Vec<KnNote> = self.kn_notes(connection_id)?.into_iter().filter(|n| n.status == NoteStatus::Approved).collect();
        Ok(plan_import(&existing, &file.notes))
    }

    /// Apply the user's choices. Returns how many notes were added or changed.
    pub fn kn_import_apply(&self, connection_id: &str, actions: &[ImportAction]) -> Result<usize> {
        let mut n = 0;
        for a in actions {
            let body = a.body.clone().unwrap_or_else(|| a.incoming.body.clone());
            match a.action.as_str() {
                "skip" => continue,
                "add" | "replace" | "merge" => {
                    if body.trim().is_empty() {
                        continue;
                    }
                    if a.action != "add" {
                        for id in &a.existing_ids {
                            // Only this connection's notes.
                            let c = self.conn.lock();
                            c.execute("DELETE FROM kn_notes WHERE id = ?1 AND connection_id = ?2", params![id, connection_id])?;
                            c.execute("DELETE FROM kn_fts WHERE source = 'note' AND ref = ?1", [id])?;
                        }
                    }
                    self.kn_save_note(KnNote {
                        id: String::new(),
                        connection_id: connection_id.to_string(),
                        target: a.target.clone().or_else(|| a.incoming.target.clone()),
                        body,
                        author: if a.action == "merge" { "merged".into() } else { a.incoming.author.clone().unwrap_or_else(|| "import".into()) },
                        status: NoteStatus::Approved,
                        created_at: 0,
                        replaces: None,
                    })?;
                    n += 1;
                }
                other => return Err(Error::Invalid(format!("unknown import action {other}"))),
            }
        }
        Ok(n)
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
    fn completion_from_the_index() {
        let ws = Workspace::open_in_memory().unwrap();
        let objs = vec![obj("s", "customer_orders", &["a"], None), obj("s", "customers", &["customer_id", "email"], None), obj("s", "big_customer", &[], None), obj("s", "x_y", &[], None), obj("s", "xay", &[], None)];
        ws.kn_replace_schema("c1", "s", &objs).unwrap();
        ws.kn_replace_schema("c1", "t", &[obj("t", "customers", &[], None)]).unwrap();
        let names = |s: Option<&str>, q: &str| ws.kn_complete("c1", s, q, 10).unwrap().into_iter().map(|r| format!("{}.{}", r.0, r.1)).collect::<Vec<_>>();
        // Prefix matches first, then shorter names.
        assert_eq!(names(Some("s"), "CUST"), vec!["s.customers", "s.customer_orders", "s.big_customer"]);
        assert_eq!(names(None, "customers"), vec!["s.customers", "t.customers"]);
        // `_` is literal, not a wildcard.
        assert_eq!(names(Some("s"), "x_"), vec!["s.x_y"]);
        assert_eq!(ws.kn_complete("c1", Some("s"), "", 2).unwrap().len(), 2);
        assert_eq!(ws.kn_column_names("c1", "s", "CUSTOMERS").unwrap().unwrap(), vec!["customer_id", "email"]);
        assert!(ws.kn_column_names("c1", "s", "nope").unwrap().is_none());
    }

    #[test]
    fn object_pages_with_filter() {
        let ws = Workspace::open_in_memory().unwrap();
        let many: Vec<KnObject> = (0..12_000).map(|i| obj("big", &format!("t{i:05}"), &["id"], (i % 1000 == 0).then_some("Orders_100% done"))).collect();
        ws.kn_replace_schema("c1", "big", &many).unwrap();
        ws.kn_replace_schema("c2", "big", &[obj("big", "other", &[], None)]).unwrap();
        let t = std::time::Instant::now();
        let (page, total) = ws.kn_list_objects("c1", "", 0, 300).unwrap();
        assert_eq!((page.len(), total), (300, 12_000));
        assert_eq!(page[0].name, "t00000");
        assert_eq!(page[0].columns.len(), 1);
        assert_eq!(ws.kn_list_objects("c1", "", 300, 2).unwrap().0[0].name, "t00300");
        // schema.name and comments, case-insensitive; `%` / `_` are literal.
        assert_eq!(ws.kn_list_objects("c1", "BIG.T0001", 0, 5).unwrap().1, 10);
        assert_eq!(ws.kn_list_objects("c1", "100% done", 0, 5).unwrap().1, 12);
        assert_eq!(ws.kn_list_objects("c1", "t_", 0, 5).unwrap().1, 0);
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}", t.elapsed());
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
        // Search rows follow removals and changes (one row per object).
        assert!(ws.kn_search("c1", "inventory", 5).unwrap().iter().all(|h| h.reference != "public.inventory"));
        let mut changed = objs[1].clone();
        changed.comment = Some("Loyalty members".into());
        let d = ws.kn_replace_schema("c1", "public", &[objs[0].clone(), changed]).unwrap();
        assert_eq!((d.changed, d.unchanged), (1, 1));
        let hits = ws.kn_search("c1", "loyalty", 5).unwrap();
        assert_eq!(hits.iter().map(|h| h.reference.as_str()).collect::<Vec<_>>(), vec!["public.customers"]);
        assert_eq!(ws.kn_search("c1", "customers", 10).unwrap().iter().filter(|h| h.reference == "public.customers").count(), 1);
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
    fn note_targets_split_into_tables() {
        let p = split_target("sales.orders & crm.customers and `x`.\"y\" OR dim.date, a.b.c");
        assert_eq!(p.iter().map(|(s, t)| (s.as_str(), t.as_str())).collect::<Vec<_>>(), vec![("", "sales.orders"), ("&", "crm.customers"), ("and", "x.y"), ("or", "dim.date"), (",", "a.b.c")]);
        assert_eq!(join_target(&p), "sales.orders & crm.customers and x.y or dim.date, a.b.c");
        assert_eq!(split_target("a.b c.d").last().unwrap().0, "&", "missing separator means &");
        assert!(split_target("  & ").is_empty());
        assert!(target_mentions("sales.orders & crm.customers.id", "crm.customers", "customers"));
        assert!(target_mentions("main.orders", "memory.main.orders", "orders"), "schema written without catalog");
        assert!(!target_mentions("sales.orders_old", "sales.orders", "orders"));
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
                replaces: None,
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
