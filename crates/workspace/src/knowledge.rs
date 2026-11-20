//! Knowledge base storage: indexed database objects, notes/glossary and a
//! full-text index (SQLite FTS5, BM25 ranking) for retrieval.

use std::collections::HashSet;

use databrain_connector_core::{ColumnInfo, ForeignKey};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Workspace, new_id, now_ms};

/// (schema, name, kind, comment) of a completion hit from the index.
pub type KnCompletion = (String, String, String, Option<String>);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct KnObject {
    pub schema: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    #[ts(as = "Option<f64>")]
    pub row_estimate: Option<i64>,
    pub columns: Vec<ColumnInfo>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
}

impl KnObject {
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    pub(crate) fn hash(&self) -> String {
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
    pub(crate) fn search_body(&self) -> String {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
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
/// A quoted name (`"…"`, `` `…` ``, `[…]`; `""` inside `"…"` is one quote)
/// is one name, dots, spaces, `&`, `,` and `and`/`or` in it included. Each
/// path comes back as [`crate::paths::path_text`] writes it: plain names
/// plain, others in double quotes. A target without a separator is one path.
pub fn split_target(target: &str) -> Vec<(String, String)> {
    // Tokens: separators (`&`, `,`, unquoted `and`/`or`) and paths, the
    // paths as written (quotes kept, read by `path_parts`).
    let mut tokens: Vec<(String, bool)> = Vec::new(); // (text, a separator)
    let mut cur = String::new();
    let mut chars = target.chars().peekable();
    let flush = |cur: &mut String, tokens: &mut Vec<(String, bool)>| {
        if !cur.is_empty() {
            let low = cur.to_ascii_lowercase();
            let sep = low == "and" || low == "or";
            tokens.push((if sep { low } else { std::mem::take(cur) }, sep));
        }
        cur.clear();
    };
    while let Some(c) = chars.next() {
        match c {
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                cur.push(c);
                while let Some(d) = chars.next() {
                    cur.push(d);
                    if d == close {
                        if close != ']' && chars.peek() == Some(&close) {
                            cur.push(chars.next().unwrap_or(close));
                            continue;
                        }
                        break;
                    }
                }
            }
            '&' | ',' => {
                flush(&mut cur, &mut tokens);
                tokens.push((c.to_string(), true));
            }
            c if c.is_whitespace() => flush(&mut cur, &mut tokens),
            c => cur.push(c),
        }
    }
    flush(&mut cur, &mut tokens);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut sep = String::new();
    for (tok, is_sep) in tokens {
        if is_sep {
            sep = tok;
            continue;
        }
        let parts = crate::paths::path_parts(&tok);
        if parts.is_empty() {
            continue;
        }
        let path = crate::paths::path_text(&parts);
        // Two paths with no separator between them: treat as "&".
        let s = if out.is_empty() {
            String::new()
        } else if sep.is_empty() {
            "&".into()
        } else {
            std::mem::take(&mut sep)
        };
        sep.clear();
        out.push((s, path));
    }
    out
}

/// Join paths back into a target, e.g. `sales.orders & crm.customers`.
/// Paths are written as [`crate::paths::table_path`] / [`split_target`]
/// give them (names quoted where needed).
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

/// Does the note target mention the table `name` of `schema` (an id, its
/// dots are levels), alone, in a list, or through one of its columns? The
/// schema may be written without its catalog (`sales.t` for `main.sales.t`).
pub fn target_mentions(target: &str, schema: &str, name: &str) -> bool {
    let lower = |v: Vec<String>| v.into_iter().map(|p| p.to_lowercase()).collect::<Vec<_>>();
    let mut full: Vec<String> = schema.split('.').filter(|s| !s.is_empty()).map(str::to_lowercase).collect();
    full.push(name.to_lowercase());
    split_target(target).iter().any(|(_, p)| {
        let p = lower(crate::paths::path_parts(p));
        // A suffix of the table's path (bare name, schema without catalog),
        // or the whole path and a column.
        (!p.is_empty() && p.len() <= full.len() && full.ends_with(&p)) || (p.len() == full.len() + 1 && p.starts_with(&full))
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct NotesSource {
    pub connection: String,
    pub kind: String,
}

/// One note in a file (no ids: they are local).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct NoteEntry {
    #[serde(default)]
    pub target: Option<String>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct ImportAction {
    pub incoming: NoteEntry,
    #[ts(type = "\"add\" | \"skip\" | \"replace\" | \"merge\"")]
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
            let identical = existing.iter().any(|e| {
                normalize(&e.body) == body
                    && e.target.as_deref().map(str::to_lowercase) == n.target.as_deref().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty())
            });
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
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
    #[ts(as = "f64")]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct KnState {
    pub connection_id: String,
    #[ts(as = "f64")]
    pub indexed_at: i64,
    #[ts(as = "f64")]
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
    "the", "and", "for", "with", "from", "that", "this", "what", "which", "show", "me", "all", "of", "in", "by", "to", "is", "are", "was", "how", "many",
    "each", "per", "a", "an", "on", "at", "give", "list", "get", "find", "select", "where", "top", "last", "first", "please", "can", "you", "my", "our",
    "query", "sql", "write",
];

impl Workspace {
    /// The objects of one schema as an index run read them in full: their
    /// facts go to the explorer cache, the one store the AI reads
    /// (`ai_objects`); its search rows follow from there. A table the read
    /// doesn't have is gone from the server, as with a filled explorer
    /// listing. A read that found no table answers for none of them (the
    /// user may see nothing there): as with an empty explorer listing, the
    /// tables leave the cache and the AI keeps what it may use of them.
    pub fn kn_replace_schema(&self, connection_id: &str, schema: &str, objects: &[KnObject]) -> Result<IndexDelta> {
        self.kn_replace_schema_read(connection_id, schema, objects, false, true)
    }

    /// [`Self::kn_replace_schema`]; `keys_read`: the read covered foreign
    /// keys, so a table's list replaces the cached one, an empty list too
    /// (keys dropped on the server go). Otherwise keys read before stay.
    /// `complete`: the read had no error, so a table it lacks is gone; a
    /// read that failed partway removes nothing.
    pub fn kn_replace_schema_read(&self, connection_id: &str, schema: &str, objects: &[KnObject], keys_read: bool, complete: bool) -> Result<IndexDelta> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let before = Self::known_tx(&tx, connection_id, Some(schema))?;
        let now = now_ms();
        let mut delta = IndexDelta::default();
        if objects.is_empty() && complete && !before.is_empty() {
            Self::keep_tx(&tx, connection_id, Some(schema))?;
            for (_, name) in before.keys() {
                tx.execute(
                    "DELETE FROM meta_objects WHERE connection_id = ?1 AND schema_name = ?2 AND name = ?3 \
                       AND kind IN ('table', 'view', 'materialized_view', 'foreign_table')",
                    params![connection_id, schema, name],
                )?;
                tx.execute("DELETE FROM meta_columns WHERE connection_id = ?1 AND schema_name = ?2 AND table_name = ?3", params![connection_id, schema, name])?;
                delta.removed += 1;
            }
        }
        if !objects.is_empty() && complete {
            let keep: HashSet<&str> = objects.iter().map(|o| o.name.as_str()).collect();
            for name in before.keys().map(|(_, n)| n).filter(|n| !keep.contains(n.as_str())) {
                tx.execute(
                    "DELETE FROM meta_objects WHERE connection_id = ?1 AND schema_name = ?2 AND name = ?3 \
                       AND kind IN ('table', 'view', 'materialized_view', 'foreign_table')",
                    params![connection_id, schema, name],
                )?;
                tx.execute("DELETE FROM meta_columns WHERE connection_id = ?1 AND schema_name = ?2 AND table_name = ?3", params![connection_id, schema, name])?;
                delta.removed += 1;
            }
        }
        for o in objects {
            let old = before.get(&(o.schema.clone(), o.name.clone()));
            Self::meta_put_known_tx(&tx, connection_id, o, now, keys_read)?;
            if Self::changes(old, o, keys_read) {
                delta.changed += 1;
            } else {
                delta.unchanged += 1;
            }
        }
        // Indexing a schema lets the AI use it. An existing rule (the user
        // excluded the schema) is kept.
        if !objects.is_empty() {
            Self::put_rule_if_absent(&tx, connection_id, &crate::ScopeRule { schema: schema.to_string(), table: String::new(), allow: true })?;
            // A read that found tables and had no error answers for the
            // schema's kept ones.
            if complete {
                Self::kept_answered_tx(&tx, connection_id, schema)?;
            }
        }
        Self::ai_refresh_tx(&tx, connection_id, Some(schema))?;
        tx.commit()?;
        Ok(delta)
    }

    /// What the explorer cache knows of a schema's tables (every schema's
    /// with `None`), by (schema, name): the table before a view of the same name.
    fn known_tx(c: &rusqlite::Connection, connection_id: &str, schema: Option<&str>) -> Result<std::collections::HashMap<(String, String), KnObject>> {
        let mut out = std::collections::HashMap::new();
        for o in Self::facts_tx(c, connection_id, schema, None)? {
            out.entry((o.schema.clone(), o.name.clone())).or_insert(o);
        }
        Ok(out)
    }

    /// Whether writing `o` ([`Self::meta_put_known_tx`]) changes what the
    /// cache had (`old`): columns not read and keys not read keep the
    /// cached ones, as does a row estimate not read.
    fn changes(old: Option<&KnObject>, o: &KnObject, keys_read: bool) -> bool {
        let Some(old) = old else { return true };
        let mut now = o.clone();
        if now.columns.is_empty() {
            now.columns = old.columns.clone();
            now.foreign_keys = old.foreign_keys.clone();
        } else if now.foreign_keys.is_empty() && !keys_read {
            now.foreign_keys = old.foreign_keys.clone();
        }
        if now.row_estimate.is_none() {
            now.row_estimate = old.row_estimate;
        }
        now.hash() != old.hash()
    }

    /// An indexed object's facts into the explorer cache: the object, and
    /// its columns and foreign keys as read now.
    fn meta_put_known_tx(tx: &rusqlite::Transaction<'_>, connection_id: &str, o: &KnObject, now: i64, keys_read: bool) -> Result<()> {
        // One table-like object per name: a table that became a view isn't both.
        tx.prepare_cached(
            "DELETE FROM meta_objects WHERE connection_id = ?1 AND schema_name = ?2 AND name = ?3 AND kind <> ?4 \
               AND kind IN ('table', 'view', 'materialized_view', 'foreign_table')",
        )?
        .execute(params![connection_id, o.schema, o.name, o.kind])?;
        tx.prepare_cached(
            "INSERT INTO meta_objects (connection_id, schema_name, name, kind, comment, row_estimate, seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (connection_id, schema_name, kind, name) DO UPDATE SET comment = excluded.comment, \
             row_estimate = coalesce(excluded.row_estimate, row_estimate), seen_at = excluded.seen_at",
        )?
        .execute(params![connection_id, o.schema, o.name, o.kind, o.comment, o.row_estimate, now])?;
        // No columns: the run couldn't read them (a failed column query
        // gives the object alone); what the explorer has is kept. No keys:
        // when the read didn't cover them (many engines' bulk reads), keys
        // read before (a describe) are kept; when it did, the table has none.
        if o.columns.is_empty() {
            return Ok(());
        }
        tx.prepare_cached(
            "INSERT INTO meta_columns (connection_id, schema_name, table_name, columns_json, seen_at, fks_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (connection_id, schema_name, table_name) DO UPDATE SET columns_json = excluded.columns_json, \
             seen_at = excluded.seen_at, fks_json = CASE WHEN excluded.fks_json = '[]' AND NOT ?7 THEN fks_json ELSE excluded.fks_json END, stale = 0",
        )?
        .execute(params![
            connection_id,
            o.schema,
            o.name,
            serde_json::to_string(&o.columns)?,
            now,
            serde_json::to_string(&o.foreign_keys)?,
            keys_read
        ])?;
        Ok(())
    }

    /// Add or refresh single objects (indexing chosen tables), keeping the
    /// rest of their schemas. Returns how many changed.
    pub fn kn_upsert_objects(&self, connection_id: &str, objects: &[KnObject]) -> Result<usize> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let now = now_ms();
        let mut changed = 0;
        let schemas: HashSet<&str> = objects.iter().map(|o| o.schema.as_str()).collect();
        for o in objects {
            // Only this table's facts (not its whole schema's).
            let old = Self::facts_tx(&tx, connection_id, Some(&o.schema), Some(&o.name))?.into_iter().next();
            changed += usize::from(Self::changes(old.as_ref(), o, false));
            Self::meta_put_known_tx(&tx, connection_id, o, now, false)?;
        }
        // Indexing a table lets the AI use it.
        // An existing rule (the user excluded the table) is kept.
        for o in objects {
            Self::put_rule_if_absent(&tx, connection_id, &crate::ScopeRule { schema: o.schema.clone(), table: o.name.clone(), allow: true })?;
        }
        for s in schemas {
            Self::ai_refresh_tx(&tx, connection_id, Some(s))?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Fingerprints of the schemas as last read in full (see `Session::explorer_fingerprints`).
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
        // The schemas earlier runs indexed: the last run's list and those
        // with a fingerprint, both as the server listed them (a scope rule
        // may be written in another case, and must not be taken for one).
        let mut current: Vec<String> = self.kn_state(connection_id)?.map(|s| s.schemas).unwrap_or_default();
        current.extend(self.kn_fingerprints(connection_id)?.into_keys());
        current.sort();
        current.dedup();
        let gone: Vec<String> = current.into_iter().filter(|s| !schemas.contains(s)).collect();
        // Out of the index: out of the AI's scope.
        let mut removed = 0;
        if !gone.is_empty() {
            removed = self.ai_catalog(connection_id).count_in(&gone)?;
            self.ai_remove_schemas(connection_id, &gone)?;
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
        Ok(c.query_row("SELECT connection_id, indexed_at, objects, schemas_json, error FROM kn_state WHERE connection_id = ?1", [connection_id], |r| {
            Ok(KnState {
                connection_id: r.get(0)?,
                indexed_at: r.get(1)?,
                objects: r.get(2)?,
                schemas: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                error: r.get(4)?,
            })
        })
        .optional()?)
    }

    /// Completion from the local index (no network): tables/views whose name
    /// contains `query` (empty = any), in `schema` when given, prefix matches
    /// and short names first. Returns (schema, name, kind, comment).
    pub fn kn_complete(&self, connection_id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<KnCompletion>> {
        let c = self.conn.lock();
        let q = query.trim().to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let mut stmt = c.prepare_cached(
            // The explorer cache: every table DataBrain knows (index runs write it too).
            "SELECT schema_name, name, max(kind), max(comment) FROM meta_objects WHERE connection_id = ?1 \
                   AND kind IN ('table', 'view', 'materialized_view', 'foreign_table') \
               AND (?2 IS NULL OR schema_name = ?2) \
               AND (lower(name) LIKE '%' || ?3 || '%' ESCAPE '\\' \
                    OR (length(?3) >= 3 AND lower(coalesce(comment, '')) LIKE '%' || ?3 || '%' ESCAPE '\\')) \
             GROUP BY schema_name, name \
             ORDER BY lower(name) NOT LIKE '%' || ?3 || '%' ESCAPE '\\', lower(name) NOT LIKE ?3 || '%' ESCAPE '\\', length(name), name LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(params![connection_id, schema, q, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Column names of a table (exact schema id) from the explorer cache
    /// ([`Self::meta_table`]): user features (completion, lineage), not the AI.
    pub fn kn_column_names(&self, connection_id: &str, schema: &str, name: &str) -> Result<Option<Vec<String>>> {
        Ok(self.meta_table(connection_id, schema, name)?.map(|t| t.columns.into_iter().map(|c| c.name).collect()))
    }

    /// Forget what index runs recorded (their state and fingerprints, so
    /// the next run reads every schema) and the tables kept for the AI.
    /// What the AI may use (its scope) and the explorer cache stay, so the
    /// tables the scope allows remain the AI's.
    pub fn kn_clear(&self, connection_id: &str) -> Result<()> {
        // One transaction: all of it goes, or none.
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        Self::kn_clear_tx(&tx, connection_id)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn kn_clear_tx(tx: &rusqlite::Transaction<'_>, connection_id: &str) -> Result<()> {
        Self::kept_forget_tx(tx, connection_id)?;
        // Search rows go with `ai_tables` (kept in step); the scope stays.
        Self::ai_forget_tx(tx, connection_id)?;
        tx.execute("DELETE FROM kn_state WHERE connection_id = ?1", [connection_id])?;
        tx.execute("DELETE FROM kn_schema_state WHERE connection_id = ?1", [connection_id])?;
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
        // The note and its search row change together, or neither does.
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let n = Self::save_note_tx(&tx, n)?;
        tx.commit()?;
        Ok(n)
    }

    /// [`Self::kn_save_note`] inside the caller's transaction (`n` checked and given an id).
    fn save_note_tx(c: &rusqlite::Transaction<'_>, mut n: KnNote) -> Result<KnNote> {
        // Approving an update: the new text takes the old note's place.
        if n.status == NoteStatus::Approved
            && let Some(old) = n.replaces.take()
            && old != n.id
        {
            // Only a note of the same connection (and so only its search row).
            if c.execute("DELETE FROM kn_notes WHERE id = ?1 AND connection_id = ?2", params![old, n.connection_id])? > 0 {
                c.execute("DELETE FROM kn_fts WHERE source = 'note' AND ref = ?1", [&old])?;
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
                params![
                    n.connection_id,
                    n.id,
                    n.target.clone().unwrap_or_else(|| "glossary".into()),
                    format!("{} {}", n.target.clone().unwrap_or_default().replace(['_', '.'], " "), n.body)
                ],
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
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM kn_notes WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM kn_fts WHERE source = 'note' AND ref = ?1", [id])?;
        tx.commit()?;
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
        notes.sort_by(|a, b| {
            (a.target.is_none(), a.target.as_deref().unwrap_or(""), &a.body).cmp(&(b.target.is_none(), b.target.as_deref().unwrap_or(""), &b.body))
        });
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

    /// Apply the user's choices, all of them or none (a failure halfway
    /// left some notes replaced and others not). Returns how many notes
    /// were added or changed.
    pub fn kn_import_apply(&self, connection_id: &str, actions: &[ImportAction]) -> Result<usize> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
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
                            // Only this connection's notes (and so only their search rows).
                            if tx.execute("DELETE FROM kn_notes WHERE id = ?1 AND connection_id = ?2", params![id, connection_id])? > 0 {
                                tx.execute("DELETE FROM kn_fts WHERE source = 'note' AND ref = ?1", [id])?;
                            }
                        }
                    }
                    let target = a.target.clone().or_else(|| a.incoming.target.clone()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
                    Self::save_note_tx(
                        &tx,
                        KnNote {
                            id: new_id(),
                            connection_id: connection_id.to_string(),
                            target,
                            body,
                            author: if a.action == "merge" { "merged".into() } else { a.incoming.author.clone().unwrap_or_else(|| "import".into()) },
                            status: NoteStatus::Approved,
                            created_at: now_ms(),
                            replaces: None,
                        },
                    )?;
                    n += 1;
                }
                other => return Err(Error::Invalid(format!("unknown import action {other}"))),
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Saved queries flagged as AI examples for a connection.
    pub fn ai_examples(&self, connection_id: &str) -> Result<Vec<crate::SavedQuery>> {
        Ok(self.list_saved_queries(None)?.into_iter().filter(|q| q.ai_example && q.connection_id.as_deref().is_none_or(|c| c == connection_id)).collect())
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

    /// An index run that couldn't read a table's columns (or whose engine
    /// gives no foreign keys in bulk) keeps what the explorer knows.
    #[test]
    fn index_runs_without_columns_keep_the_cached_ones() {
        let ws = Workspace::open_in_memory().unwrap();
        let fk = ForeignKey { columns: vec!["cid".into()], ref_schema: "sales".into(), ref_table: "customers".into(), ref_columns: vec!["id".into()] };
        let cols = obj("sales", "orders", &["id", "cid"], None).columns;
        ws.meta_put_table("c", "sales", "orders", &cols, Some(std::slice::from_ref(&fk))).unwrap();
        ws.kn_replace_schema("c", "sales", &[obj("sales", "orders", &[], None)]).unwrap();
        let t = ws.meta_table("c", "sales", "orders").unwrap().unwrap();
        assert_eq!((t.columns.len(), t.foreign_keys.as_deref()), (2, Some(&[fk.clone()][..])), "columns and keys kept");
        // Columns read without keys: the columns are new, the keys kept.
        ws.kn_replace_schema("c", "sales", &[obj("sales", "orders", &["id", "cid", "total"], None)]).unwrap();
        let t = ws.meta_table("c", "sales", "orders").unwrap().unwrap();
        assert_eq!((t.columns.len(), t.foreign_keys.map(|f| f.len())), (3, Some(1)));
        // A read that covered keys and found none: the key was dropped on the server.
        ws.kn_replace_schema_read("c", "sales", &[obj("sales", "orders", &["id", "cid", "total"], None)], true, true).unwrap();
        let t = ws.meta_table("c", "sales", "orders").unwrap().unwrap();
        assert_eq!(t.foreign_keys.as_deref(), Some(&[][..]), "dropped keys go");
        // And one that found it again keeps it.
        let mut with_key = obj("sales", "orders", &["id", "cid", "total"], None);
        with_key.foreign_keys = vec![fk.clone()];
        ws.kn_replace_schema_read("c", "sales", &[with_key], true, true).unwrap();
        assert_eq!(ws.meta_table("c", "sales", "orders").unwrap().unwrap().foreign_keys, Some(vec![fk]));
    }

    #[test]
    fn completion_from_the_index() {
        let ws = Workspace::open_in_memory().unwrap();
        let objs = vec![
            obj("s", "customer_orders", &["a"], None),
            obj("s", "customers", &["customer_id", "email"], None),
            obj("s", "big_customer", &[], None),
            obj("s", "x_y", &[], None),
            obj("s", "xay", &[], None),
        ];
        ws.kn_replace_schema("c1", "s", &objs).unwrap();
        ws.kn_replace_schema("c1", "t", &[obj("t", "customers", &[], None)]).unwrap();
        let names = |s: Option<&str>, q: &str| ws.kn_complete("c1", s, q, 10).unwrap().into_iter().map(|r| format!("{}.{}", r.0, r.1)).collect::<Vec<_>>();
        // Prefix matches first, then shorter names.
        assert_eq!(names(Some("s"), "CUST"), vec!["s.customers", "s.customer_orders", "s.big_customer"]);
        assert_eq!(names(None, "customers"), vec!["s.customers", "t.customers"]);
        // `_` is literal, not a wildcard.
        assert_eq!(names(Some("s"), "x_"), vec!["s.x_y"]);
        assert_eq!(ws.kn_complete("c1", Some("s"), "", 2).unwrap().len(), 2);
        // Comments are searched too (3+ characters), after name matches.
        ws.kn_replace_schema("c1", "u", &[obj("u", "fct_rev", &[], Some("Daily revenue by country")), obj("u", "revenue_raw", &[], None)]).unwrap();
        assert_eq!(names(Some("u"), "revenue"), vec!["u.revenue_raw", "u.fct_rev"]);
        assert!(names(Some("u"), "da").is_empty(), "short text: names only");
        assert_eq!(names(Some("u"), "daily"), vec!["u.fct_rev"]);
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
        let (page, total) = ws.ai_catalog("c1").page("", 0, 300).unwrap();
        assert_eq!((page.len(), total), (300, 12_000));
        assert_eq!(page[0].name, "t00000");
        assert_eq!(page[0].columns.len(), 1);
        assert_eq!(ws.ai_catalog("c1").page("", 300, 2).unwrap().0[0].name, "t00300");
        // schema.name and comments, case-insensitive; `%` / `_` are literal.
        assert_eq!(ws.ai_catalog("c1").page("BIG.T0001", 0, 5).unwrap().1, 10);
        assert_eq!(ws.ai_catalog("c1").page("100% done", 0, 5).unwrap().1, 12);
        assert_eq!(ws.ai_catalog("c1").page("t_", 0, 5).unwrap().1, 0);
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

        let hits = ws.ai_catalog("c1").search("revenue by customer orders", 5).unwrap();
        assert_eq!(hits[0].reference, "public.customer_orders");
        // Search rows follow removals and changes (one row per object).
        assert!(ws.ai_catalog("c1").search("inventory", 5).unwrap().iter().all(|h| h.reference != "public.inventory"));
        let mut changed = objs[1].clone();
        changed.comment = Some("Loyalty members".into());
        let d = ws.kn_replace_schema("c1", "public", &[objs[0].clone(), changed]).unwrap();
        assert_eq!((d.changed, d.unchanged), (1, 1));
        let hits = ws.ai_catalog("c1").search("loyalty", 5).unwrap();
        assert_eq!(hits.iter().map(|h| h.reference.as_str()).collect::<Vec<_>>(), vec!["public.customers"]);
        assert_eq!(ws.ai_catalog("c1").search("customers", 10).unwrap().iter().filter(|h| h.reference == "public.customers").count(), 1);
        assert!(ws.ai_catalog("c1").search("", 5).unwrap().is_empty());
        assert!(ws.ai_catalog("other").search("orders", 5).unwrap().is_empty());
        // FTS syntax characters are neutralized.
        assert!(ws.ai_catalog("c1").search("\"orders\" AND (x OR", 5).is_ok());

        assert_eq!(ws.ai_catalog("c1").get("CUSTOMERS").unwrap().unwrap().columns.len(), 2);
        assert!(ws.ai_catalog("c1").get("public.customers").unwrap().is_some());
        assert!(ws.ai_catalog("c1").get("nope").unwrap().is_none());

        ws.kn_set_state(&KnState { connection_id: "c1".into(), indexed_at: 1, objects: 2, schemas: vec!["public".into()], error: None }).unwrap();
        assert_eq!(ws.kn_retain_schemas("c1", &[]).unwrap(), 2, "out of the index: out of the AI's scope");
        assert_eq!(ws.ai_catalog("c1").count().unwrap(), 0);
    }

    /// An index read that finds a schema empty (no error) is no answer
    /// about each table: they leave the cache, the AI keeps them (marked)
    /// until a read finds tables again or they expire.
    #[test]
    fn an_empty_index_read_keeps_the_tables_for_the_ai() {
        let ws = Workspace::open_in_memory().unwrap();
        let objs = [obj("s", "a", &["id"], None), obj("s", "b", &["id"], None)];
        ws.kn_replace_schema("c1", "s", &objs).unwrap();
        let d = ws.kn_replace_schema("c1", "s", &[]).unwrap();
        assert_eq!(d.removed, 2);
        assert!(ws.meta_table("c1", "s", "a").unwrap().is_none() && ws.meta_get("c1", "s.a").unwrap().is_none(), "not in the explorer");
        let ai = ws.ai_catalog("c1");
        assert_eq!(ai.count().unwrap(), 2);
        assert!(ai.missing_since("s", "a").unwrap().is_some());
        assert_eq!(ai.get("s.b").unwrap().unwrap().columns.len(), 1, "with its columns");
        // With an error: nothing changes (it was already kept here).
        ws.kn_replace_schema_read("c1", "s", &[], false, false).unwrap();
        assert_eq!(ai.count().unwrap(), 2);
        // Found again: the cache's, unmarked.
        ws.kn_replace_schema("c1", "s", &objs[..1]).unwrap();
        assert_eq!(ai.count().unwrap(), 1, "b: a read with tables answers for it");
        assert!(ai.missing_since("s", "a").unwrap().is_none());
        // A schema the AI may not use keeps nothing.
        ws.ai_put_rule("c1", &crate::ScopeRule { schema: "s".into(), table: String::new(), allow: false }).unwrap();
        ws.kn_replace_schema("c1", "s", &[]).unwrap();
        assert_eq!(ws.conn.lock().query_row("SELECT count(*) FROM ai_kept", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    /// A read that failed partway (some tables, an error) adds what it has
    /// and removes nothing: the tables it lacks may still be on the server.
    #[test]
    fn a_read_with_an_error_removes_nothing() {
        let ws = Workspace::open_in_memory().unwrap();
        let objs = [obj("s", "a", &["id"], None), obj("s", "b", &["id"], None)];
        ws.kn_replace_schema("c1", "s", &objs).unwrap();
        ws.meta_clear("c1").unwrap();
        let kept = || ws.conn.lock().query_row("SELECT count(*) FROM ai_kept", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(kept(), 2, "the cache was cleared: kept for the AI");
        let d = ws.kn_replace_schema_read("c1", "s", &objs[..1], false, false).unwrap();
        assert_eq!(d.removed, 0);
        assert_eq!(kept(), 1, "b is still kept: the read didn't answer for it");
        assert!(ws.ai_catalog("c1").get("s.b").unwrap().is_some());
        // Read in full without b: gone.
        let d = ws.kn_replace_schema_read("c1", "s", &objs[..1], false, true).unwrap();
        assert_eq!((kept(), ws.ai_catalog("c1").get("s.b").unwrap().is_none()), (0, true));
        assert_eq!(d.removed, 0, "b was no longer in the cache, only kept");
        ws.kn_replace_schema("c1", "s", &objs).unwrap();
        assert_eq!(ws.kn_replace_schema_read("c1", "s", &objs[..1], false, false).unwrap().removed, 0);
        assert!(ws.meta_table("c1", "s", "b").unwrap().is_some());
        assert_eq!(ws.kn_replace_schema("c1", "s", &objs[..1]).unwrap().removed, 1);
        assert!(ws.meta_table("c1", "s", "b").unwrap().is_none());
    }

    #[test]
    fn note_targets_split_into_tables() {
        let p = split_target("sales.orders & crm.customers and `x`.\"y\" OR dim.date, a.b.c");
        assert_eq!(
            p.iter().map(|(s, t)| (s.as_str(), t.as_str())).collect::<Vec<_>>(),
            vec![("", "sales.orders"), ("&", "crm.customers"), ("and", "x.y"), ("or", "dim.date"), (",", "a.b.c")]
        );
        assert_eq!(join_target(&p), "sales.orders & crm.customers and x.y or dim.date, a.b.c");
        assert_eq!(split_target("a.b c.d").last().unwrap().0, "&", "missing separator means &");
        assert!(split_target("  & ").is_empty());
        assert!(target_mentions("sales.orders & crm.customers.id", "crm", "customers"));
        assert!(target_mentions("main.orders", "memory.main", "orders"), "schema written without catalog");
        assert!(target_mentions("orders", "sales", "orders"), "bare name");
        assert!(!target_mentions("sales.orders_old", "sales", "orders"));
    }

    /// Names with a dot, a space, `&`, `,`, a quote or the word `and` are
    /// one name when quoted, and stored quoted: they read back the same.
    #[test]
    fn note_targets_keep_unusual_names() {
        use crate::paths::table_path;
        let parts = vec![
            (String::new(), table_path("sales", "order items", None)),
            ("&".to_string(), table_path("crm", "R&D, old", None)),
            ("and".to_string(), table_path("dim", "and", None)),
            ("or".to_string(), table_path("x", "say \"hi\"", None)),
            ("&".to_string(), table_path("sales", "orders.2024", Some("amount"))),
        ];
        let stored = join_target(&parts);
        assert_eq!(stored, r#"sales."order items" & crm."R&D, old" and dim."and" or x."say ""hi""" & sales."orders.2024".amount"#);
        assert_eq!(split_target(&stored), parts, "reads back as written");
        assert!(target_mentions(&stored, "sales", "order items"));
        assert!(target_mentions(&stored, "crm", "R&D, old"));
        assert!(target_mentions(&stored, "sales", "orders.2024"), "through its column");
        assert!(!target_mentions(&stored, "sales.orders", "2024"), "not a table `2024` of `sales.orders`");
        // Brackets and backticks are read too, and written in double quotes.
        assert_eq!(split_target("[my schema].`my table` & a.b")[0].1, r#""my schema"."my table""#);
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
        assert!(ws.ai_catalog("c1").search("recurring revenue", 5).unwrap().is_empty());
        let mut a = n.clone();
        a.status = NoteStatus::Approved;
        ws.kn_save_note(a).unwrap();
        let hits = ws.ai_catalog("c1").search("recurring revenue", 5).unwrap();
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
