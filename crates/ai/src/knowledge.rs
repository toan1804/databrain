//! Database metadata as AI knowledge: indexing schemas into the workspace
//! store and retrieving the most relevant tables (BM25 + foreign-key
//! expansion) rendered as compact DDL for prompts.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use databrain_query_engine::QueryEngine;
use databrain_workspace::{ConnectionProfile, KnObject, KnState, LearnedJoin, NoteStatus, Workspace, join_target, now_ms, split_target, target_mentions};
use serde::Serialize;

use crate::catalog::AiTables;
use crate::learn;
use crate::policy::is_pii;
use crate::types::{AiError, Result};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize)]
pub struct IndexReport {
    pub schemas: usize,
    pub objects: usize,
    pub changed: usize,
    pub removed: usize,
    pub errors: Vec<String>,
    /// Stopped by the user; schemas indexed before that are kept.
    pub cancelled: bool,
    /// Schemas not re-read because their definitions did not change since
    /// the last run (engines with schema fingerprints: Postgres, Oracle).
    #[serde(default)]
    pub skipped: usize,
}

/// Progress callback: (schema, done, total).
pub type Progress<'a> = &'a (dyn Fn(&str, usize, usize) + Send + Sync);

fn system_schema(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let last = n.rsplit('.').next().unwrap_or(&n);
    matches!(last, "information_schema" | "pg_catalog" | "pg_toast" | "sys" | "mysql" | "performance_schema" | "sysibm" | "ctxsys" | "mdsys" | "xdb" | "outln")
        || last.starts_with("pg_temp")
}

/// Does a schema id match an index scope entry? Entries are schema ids
/// (`main.sales`, `public`), `catalog.*` for a whole catalog, or `*`.
pub fn scope_matches(scope: &[String], schema: &str) -> bool {
    scope.iter().any(|w| {
        let w = w.trim();
        w == "*"
            || w.eq_ignore_ascii_case(schema)
            || w.strip_suffix(".*").is_some_and(|c| schema.len() > c.len() && schema[..c.len()].eq_ignore_ascii_case(c) && schema.as_bytes()[c.len()] == b'.')
    })
}

/// Above these sizes the UI asks which catalogs/schemas to index.
pub const LARGE_SCHEMAS: usize = 25;
pub const LARGE_CATALOGS: usize = 3;
pub const LARGE_OBJECTS: usize = 1000;

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct PlanSchema {
    /// Schema id (`catalog.schema` for three-level engines).
    pub name: String,
    pub catalog: Option<String>,
    pub is_default: bool,
    /// information_schema, pg_catalog …: not indexed unless chosen.
    pub system: bool,
    /// Tables/views, when the engine can count them cheaply.
    pub objects: Option<usize>,
    /// In the saved scope (or, with no saved scope, indexed by default).
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct IndexPlan {
    pub schemas: Vec<PlanSchema>,
    /// Saved scope (`ai_policy.index_schemas`); empty = never chosen (all
    /// non-system schemas), `["*"]` = all chosen explicitly.
    pub scope: Vec<String>,
    pub catalogs: usize,
    pub total_objects: Option<usize>,
    /// Big enough that the user should pick what to index.
    pub large: bool,
    /// Saved schemas-per-batch setting.
    pub batch: usize,
}

/// What an index run would cover: one schema listing (+ one count query
/// where supported). No per-schema queries.
pub async fn plan(engine: &Arc<QueryEngine>, connection_id: &str) -> Result<IndexPlan> {
    let profile = engine.workspace().get_connection(connection_id)?;
    let scope = profile.ai_policy.index_schemas.clone();
    let list = engine.list_schemas(connection_id).await?;
    let counts = engine.schema_object_counts(connection_id).await.ok().flatten();
    let schemas: Vec<PlanSchema> = list
        .into_iter()
        .map(|s| {
            let system = system_schema(&s.name);
            PlanSchema {
                selected: if scope.is_empty() || scope.iter().all(|w| w.trim() == "*") { !system } else { scope_matches(&scope, &s.name) },
                objects: counts.as_ref().map(|c| c.get(&s.name).copied().unwrap_or(0)),
                system,
                catalog: s.catalog,
                is_default: s.is_default,
                name: s.name,
            }
        })
        .collect();
    let catalogs = schemas.iter().filter_map(|s| s.catalog.as_deref()).collect::<HashSet<_>>().len();
    let total_objects = counts.map(|_| schemas.iter().filter(|s| !s.system).filter_map(|s| s.objects).sum());
    let user = schemas.iter().filter(|s| !s.system).count();
    let large = user > LARGE_SCHEMAS || catalogs > LARGE_CATALOGS || total_objects.is_some_and(|n| n > LARGE_OBJECTS);
    let batch = (profile.ai_policy.index_batch as usize).clamp(1, MAX_BATCH);
    Ok(IndexPlan { schemas, scope, catalogs, total_objects, large, batch })
}

/// Default schemas per metadata batch: progress and cancellation happen
/// between batches (engines with bulk metadata use ~2 queries per batch).
pub const DEFAULT_BATCH: usize = 25;
/// Upper bound for `ai_policy.index_batch`.
pub const MAX_BATCH: usize = 500;

/// (Re)index a connection's metadata. `scope` overrides
/// `ai_policy.index_schemas` (entries as in [`scope_matches`]; empty = all
/// non-system schemas). Stops at the next batch boundary, or immediately
/// for an in-flight request, when `cancel` fires; already indexed schemas
/// are kept.
pub async fn index_connection(
    engine: &Arc<QueryEngine>,
    connection_id: &str,
    scope: Option<&[String]>,
    progress: Progress<'_>,
    cancel: &CancellationToken,
) -> Result<IndexReport> {
    index_connection_with(engine, connection_id, scope, false, progress, cancel).await
}

/// [`index_connection`]; `full` re-reads every schema even when its
/// fingerprint says nothing changed.
pub async fn index_connection_with(
    engine: &Arc<QueryEngine>,
    connection_id: &str,
    scope: Option<&[String]>,
    full: bool,
    progress: Progress<'_>,
    cancel: &CancellationToken,
) -> Result<IndexReport> {
    // Indexing's own connection session is closed however the run ends.
    struct EndSession<'a>(&'a QueryEngine, &'a str);
    impl Drop for EndSession<'_> {
        fn drop(&mut self) {
            self.0.end_index_session(self.1);
        }
    }
    let _end = EndSession(engine, connection_id);
    let ws = engine.workspace().clone();
    let profile = ws.get_connection(connection_id)?;
    let wanted: Vec<String> = scope.map(<[String]>::to_vec).unwrap_or_else(|| profile.ai_policy.index_schemas.clone());
    let listed = tokio::select! {
        r = engine.list_schemas(connection_id) => r?,
        _ = cancel.cancelled() => return Err(AiError::Cancelled),
    };
    let schemas: Vec<String> = listed
        .into_iter()
        .map(|s| s.name)
        .filter(|s| if wanted.is_empty() || wanted.iter().all(|w| w.trim() == "*") { !system_schema(s) } else { scope_matches(&wanted, s) })
        .collect();
    let mut report = IndexReport { schemas: schemas.len(), objects: 0, changed: 0, removed: 0, errors: vec![], cancelled: false, skipped: 0 };
    // Incremental: one catalog query tells which schemas changed since the
    // last run; unchanged ones are not fetched again.
    let current: HashMap<String, String> = tokio::select! {
        r = engine.explorer_fingerprints(connection_id) => r.ok().flatten().unwrap_or_default(),
        _ = cancel.cancelled() => return Err(AiError::Cancelled),
    };
    let stored = if full { HashMap::new() } else { ws.kn_fingerprints(connection_id)? };
    let indexed_before: HashSet<String> = ws.kn_state(connection_id)?.map(|s| s.schemas.into_iter().collect()).unwrap_or_default();
    let mut done: Vec<String> = Vec::new();
    let mut todo: Vec<String> = Vec::new();
    for s in &schemas {
        match (current.get(s), stored.get(s)) {
            (Some(now), Some(then)) if now == then && indexed_before.contains(s) => {
                report.skipped += 1;
                done.push(s.clone());
            }
            _ => todo.push(s.clone()),
        }
    }
    if report.skipped > 0 {
        report.objects += AiTables::new(&ws, connection_id)?.count_in(&done)?;
    }
    let batch_size = (profile.ai_policy.index_batch as usize).clamp(1, MAX_BATCH);
    for batch in todo.chunks(batch_size) {
        if cancel.is_cancelled() {
            report.cancelled = true;
            break;
        }
        progress(&batch[0], done.len(), schemas.len());
        let metas = tokio::select! {
            r = engine.bulk_metadata(connection_id, batch) => r,
            _ = cancel.cancelled() => { report.cancelled = true; break; }
        };
        let metas = match metas {
            Ok(m) => m,
            Err(e) => {
                report.errors.push(format!("{}…: {}", batch[0], e.message));
                continue;
            }
        };
        for m in metas {
            if let Some(e) = &m.error {
                report.errors.push(format!("{}: {e}", m.schema));
                if m.objects.is_empty() {
                    continue;
                }
            }
            let items: Vec<KnObject> = m
                .objects
                .into_iter()
                .filter(|o| {
                    !matches!(
                        o.kind,
                        databrain_connector_core::ObjectKind::Function
                            | databrain_connector_core::ObjectKind::Procedure
                            | databrain_connector_core::ObjectKind::Sequence
                    )
                })
                .map(|o| {
                    let tc = m.columns.iter().find(|t| t.table == o.name);
                    KnObject {
                        schema: m.schema.clone(),
                        kind: serde_json::to_value(o.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "table".into()),
                        comment: o.comment,
                        row_estimate: o.row_estimate,
                        columns: tc.map(|t| t.columns.clone()).unwrap_or_default(),
                        foreign_keys: tc.map(|t| t.foreign_keys.clone()).unwrap_or_default(),
                        name: o.name,
                    }
                })
                .collect();
            report.objects += items.len();
            let d = ws.kn_replace_schema(connection_id, &m.schema, &items)?;
            report.changed += d.changed;
            report.removed += d.removed;
            // Fingerprint taken before the fetch: a change during the run is picked up next time.
            let fp = if m.error.is_none() { current.get(&m.schema).map(String::as_str) } else { None };
            ws.kn_set_fingerprint(connection_id, &m.schema, fp)?;
            done.push(m.schema);
        }
    }
    let indexed = if report.cancelled {
        // Partial run: keep earlier results for schemas not reached.
        let mut all: Vec<String> = ws.kn_state(connection_id)?.map(|s| s.schemas).unwrap_or_default();
        all.retain(|s| schemas.contains(s));
        for s in &done {
            if !all.contains(s) {
                all.push(s.clone());
            }
        }
        all
    } else {
        report.removed += ws.kn_retain_schemas(connection_id, &schemas)?;
        schemas.clone()
    };
    progress("", done.len(), schemas.len());
    let mut error: Vec<String> = Vec::new();
    if report.cancelled {
        error.push(format!("Indexing cancelled after {} of {} schemas", done.len(), schemas.len()));
    }
    error.extend(report.errors.iter().take(20).cloned());
    if report.errors.len() > 20 {
        error.push(format!("…and {} more errors", report.errors.len() - 20));
    }
    ws.kn_set_state(&KnState {
        connection_id: connection_id.to_string(),
        indexed_at: now_ms(),
        objects: AiTables::new(&ws, connection_id)?.count()?,
        schemas: indexed,
        error: (!error.is_empty()).then(|| error.join("; ")),
    })?;
    Ok(report)
}

/// Index chosen tables only (the AI found a table missing from the index):
/// each name is looked up (index, explorer cache, then the server), described
/// live and added to the index; the rest of the index is kept. Per name:
/// the indexed object or why it could not be indexed.
pub async fn index_tables(
    engine: &QueryEngine,
    connection_id: &str,
    names: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(String, std::result::Result<KnObject, String>)>> {
    let ws = engine.workspace().clone();
    let mut out = Vec::new();
    let mut fresh = Vec::new();
    for want in names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
        // The user approved this: wait as long as the database needs (a first
        // connect, a sign-in, a busy metadata session, a large catalog);
        // Stop cancels it. Only a stuck server gives up, after INDEX_TABLE_TIMEOUT.
        let work = async {
            let d = describe_named(engine, connection_id, want).await?;
            if !d.object.kind.is_relation() {
                return Err(format!("`{}.{}` is not a table or view", d.object.schema, d.object.name));
            }
            Ok(KnObject {
                kind: serde_json::to_value(d.object.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "table".into()),
                comment: d.object.comment,
                row_estimate: d.object.row_estimate,
                columns: d.columns,
                foreign_keys: d.foreign_keys,
                schema: d.object.schema,
                name: d.object.name,
            })
        };
        let r: std::result::Result<KnObject, String> = tokio::select! {
            r = tokio::time::timeout(INDEX_TABLE_TIMEOUT, work) => r.unwrap_or_else(|_| Err(format!("no answer from the database in {} s", INDEX_TABLE_TIMEOUT.as_secs()))),
            _ = cancel.cancelled() => return Err(AiError::Cancelled),
        };
        if let Ok(o) = &r {
            fresh.push(o.clone());
        }
        out.push((want.to_string(), r));
    }
    if !fresh.is_empty() {
        ws.kn_upsert_objects(connection_id, &fresh)?;
        let objects = AiTables::new(&ws, connection_id)?.count()?;
        let st = ws.kn_state(connection_id)?.unwrap_or(KnState {
            connection_id: connection_id.to_string(),
            indexed_at: 0,
            objects: 0,
            schemas: vec![],
            error: None,
        });
        ws.kn_set_state(&KnState { objects, indexed_at: if st.indexed_at == 0 { now_ms() } else { st.indexed_at }, ..st })?;
    }
    Ok(out)
}

/// Per table of `index_tables`: generous, it only stops a server that hangs.
const INDEX_TABLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(if cfg!(test) { 5 } else { 300 });

/// Describe `schema.table` (or a bare `table`) as written: a qualified name is
/// described directly (no catalog search); otherwise, or when that fails
/// (different case, a name only the search knows), it is looked up first.
async fn describe_named(engine: &QueryEngine, connection_id: &str, want: &str) -> std::result::Result<databrain_connector_core::ObjectDetail, String> {
    if let Some((schema, name)) = want.rsplit_once('.').filter(|(s, n)| !s.is_empty() && !n.is_empty())
        && let Ok(d) = engine.describe(connection_id, schema, name).await
    {
        return Ok(d);
    }
    let (schema, name) = resolve_table(engine, connection_id, want).await?.ok_or_else(|| format!("no table or view named `{want}` on this connection"))?;
    engine.describe(connection_id, &schema, &name).await.map_err(|e| e.message)
}

/// Compact DDL-like description for prompts. PII columns are listed but
/// flagged so the model avoids selecting them.
pub fn render_object(o: &KnObject, pii: &dyn Fn(&str) -> bool, notes: &[String]) -> String {
    let mut s = format!("-- {} {}", o.kind, o.full_name());
    if let Some(n) = o.row_estimate.filter(|n| *n > 0) {
        s.push_str(&format!(" (~{n} rows)"));
    }
    if let Some(c) = o.comment.as_deref().filter(|c| !c.is_empty()) {
        s.push_str(&format!(": {}", one_line(c, 200)));
    }
    s.push('\n');
    s.push_str(&format!("{} (\n", o.full_name()));
    let cols: Vec<String> = o
        .columns
        .iter()
        .map(|c| {
            let mut l = format!("  {} {}", c.name, c.data_type);
            if c.is_primary_key {
                l.push_str(" PK");
            }
            if !c.nullable {
                l.push_str(" NOT NULL");
            }
            if pii(&c.name) {
                l.push_str(" [PII: do not select]");
            }
            if let Some(cm) = c.comment.as_deref().filter(|c| !c.is_empty()) {
                l.push_str(&format!(" -- {}", one_line(cm, 120)));
            }
            l
        })
        .collect();
    s.push_str(&cols.join(",\n"));
    s.push_str("\n)");
    for fk in &o.foreign_keys {
        s.push_str(&format!("\n  FK ({}) -> {}.{}({})", fk.columns.join(", "), fk.ref_schema, fk.ref_table, fk.ref_columns.join(", ")));
    }
    for n in notes {
        s.push_str(&format!("\n  NOTE: {}", one_line(n, 300)));
    }
    s
}

fn one_line(s: &str, n: usize) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > n { t.chars().take(n).collect::<String>() + "…" } else { t }
}

#[derive(Debug, Clone, Serialize)]
pub struct Retrieved {
    pub tables: Vec<String>,
    pub text: String,
    pub notes: Vec<String>,
    /// Learned `JOIN` lines in `text`.
    pub learned_joins: usize,
}

/// Retrieve relevant schema context for `question` within ~`budget_chars`.
pub fn retrieve(ws: &Workspace, connection_id: &str, question: &str, max_tables: usize, budget_chars: usize) -> Result<Retrieved> {
    let profile = ws.get_connection(connection_id)?;
    let policy = profile.ai_policy.clone();
    let t = AiTables::with_profile(ws, &profile);
    let notes_all = ws.kn_notes(connection_id)?.into_iter().filter(|n| n.status == NoteStatus::Approved).collect::<Vec<_>>();
    let mut hits = t.search(question, 40)?;
    // Tables the user's queries use often rank a little higher (BM25
    // scores are negative: lower is better).
    if policy.learn_from_history {
        let usage: HashMap<String, i64> = ws.table_usage(connection_id)?.into_iter().map(|u| (u.table.to_lowercase(), u.queries)).collect();
        if !usage.is_empty() {
            let adj = |h: &databrain_workspace::KnHit| {
                let q = if h.source == "note" { 0 } else { usage.get(&h.reference.to_lowercase()).copied().unwrap_or(0) };
                h.score * (1.0 + 0.2 * (1.0 + q as f64).ln())
            };
            hits.sort_by(|a, b| adj(a).total_cmp(&adj(b)));
        }
    }
    let learned = usable_joins(ws, &profile)?;
    let mut chosen: Vec<KnObject> = Vec::new();
    let mut seen = HashSet::new();
    let mut glossary: Vec<String> = Vec::new();
    for h in &hits {
        if h.source == "note" {
            if let Some(n) = notes_all.iter().find(|n| n.id == h.reference) {
                if n.target.is_none() {
                    glossary.push(n.body.clone());
                } else {
                    // Every table the note is about (`a & b`, `a or b`).
                    for (_, p) in split_target(n.target.as_deref().unwrap_or("")) {
                        if let Some((o, _)) = t.resolve_path(&p)?
                            && seen.insert(o.full_name())
                        {
                            chosen.push(o);
                        }
                    }
                }
            }
            continue;
        }
        if chosen.len() >= max_tables {
            break;
        }
        if let Some(o) = t.get(&h.reference)?
            && seen.insert(o.full_name())
        {
            chosen.push(o);
        }
    }
    // Small databases: include everything when nothing matched.
    if chosen.is_empty() && t.count()? <= max_tables as i64 {
        for o in t.objects()? {
            if seen.insert(o.full_name()) {
                chosen.push(o);
            }
        }
    }
    // FK expansion: add tables referenced by the top hits (one hop).
    let top: Vec<KnObject> = chosen.iter().take(4).cloned().collect();
    for o in top {
        for fk in &o.foreign_keys {
            if chosen.len() >= max_tables + 3 {
                break;
            }
            if let Some(r) = t.get(&format!("{}.{}", fk.ref_schema, fk.ref_table))?
                && seen.insert(r.full_name())
            {
                chosen.push(r);
            }
        }
    }
    // Learned joins: the most used partners of the top tables (one hop).
    let top: Vec<String> = chosen.iter().take(4).map(|o| o.full_name()).collect();
    for name in &top {
        for j in &learned {
            if chosen.len() >= max_tables + 3 {
                break;
            }
            let Some(sd) = side(j, name) else { continue };
            if let Some(r) = t.get(sd.other)?
                && seen.insert(r.full_name())
            {
                chosen.push(r);
            }
        }
    }
    let mut text = String::new();
    let mut tables = Vec::new();
    let mut learned_joins = 0;
    for o in &chosen {
        let full = o.full_name();
        let table_notes: Vec<String> = notes_all
            .iter()
            .filter(|n| n.target.as_deref().is_some_and(|t| target_mentions(t, &full, &o.name)))
            .map(|n| match n.target.as_deref() {
                Some(t) if !t.eq_ignore_ascii_case(&full) => format!("{t}: {}", n.body),
                _ => n.body.clone(),
            })
            .collect();
        let pii = |c: &str| is_pii(&policy, Some(&full), c);
        let mut block =
            if policy.share_metadata { render_object(o, &pii, &table_notes) } else { format!("-- {} {} (metadata sharing disabled)", o.kind, full) };
        let lines = join_lines(&full, &learned);
        for l in &lines {
            block.push('\n');
            block.push_str(l);
        }
        if !text.is_empty() && text.len() + block.len() > budget_chars {
            break;
        }
        text.push_str(&block);
        text.push_str("\n\n");
        tables.push(full);
        learned_joins += lines.len();
    }
    Ok(Retrieved { tables, text: text.trim_end().to_string(), notes: glossary, learned_joins })
}

// ------------------------------------------------------------ learned joins

/// Learned joins the AI may see on this connection: learning on and
/// metadata shared; seen often enough or confirmed ([`learn::surfaced`]);
/// not a declared foreign key (shown as `FK` already); both tables readable
/// under the table access rules; and every column still in its table, when
/// the table's columns are known.
pub fn usable_joins(ws: &Workspace, profile: &ConnectionProfile) -> Result<Vec<LearnedJoin>> {
    let p = &profile.ai_policy;
    if !p.share_metadata {
        return Ok(vec![]);
    }
    // With learning from history off, only documented joins.
    let learning = p.learn_from_history;
    let t = AiTables::with_profile(ws, profile);
    let mut known: HashMap<String, Option<Vec<String>>> = HashMap::new();
    let mut has = |table: &str, cols: &[String]| -> bool {
        let k = known.entry(table.to_lowercase()).or_insert_with(|| {
            let (schema, name) = table.rsplit_once('.')?;
            t.column_names(schema, name).ok().flatten()
        });
        k.as_ref().is_none_or(|k| cols.iter().all(|c| k.iter().any(|x| x.eq_ignore_ascii_case(c))))
    };
    // Both tables must be ones the AI may use: a join names them.
    let readable = |name: &str| t.get(name).ok().flatten().is_some();
    Ok(ws
        .learned_joins(&profile.id)?
        .into_iter()
        .filter(|j| if learning { learn::surfaced(j) } else { j.status != databrain_workspace::JoinStatus::Rejected && learn::documented(j) })
        .filter(|j| !j.declared && readable(&j.left_table) && readable(&j.right_table))
        .filter(|j| has(&j.left_table, &j.left_columns) && has(&j.right_table, &j.right_columns))
        .collect())
}

/// A learned join as seen from one of its tables.
struct Side<'j> {
    columns: &'j [String],
    other: &'j str,
    other_columns: &'j [String],
    /// Join kind → distinct queries, as seen from this table.
    kinds: Vec<(String, i64)>,
}

/// `table`'s side of a learned join.
fn side<'j>(j: &'j LearnedJoin, table: &str) -> Option<Side<'j>> {
    let kinds = |flip: bool| -> Vec<(String, i64)> {
        j.kinds
            .iter()
            .map(|(k, n)| {
                let k = match (flip, k.as_str()) {
                    (true, "left") => "right".to_string(),
                    (true, "right") => "left".to_string(),
                    _ => k.clone(),
                };
                (k, *n)
            })
            .collect()
    };
    if j.left_table.eq_ignore_ascii_case(table) {
        Some(Side { columns: &j.left_columns, other: &j.right_table, other_columns: &j.right_columns, kinds: kinds(false) })
    } else if j.right_table.eq_ignore_ascii_case(table) {
        Some(Side { columns: &j.right_columns, other: &j.left_table, other_columns: &j.left_columns, kinds: kinds(true) })
    } else {
        None
    }
}

/// How often a learned join was used, for prompts.
fn evidence(j: &LearnedJoin) -> String {
    let used = match j.queries {
        0 => String::new(),
        1 => ", used in 1 past query".into(),
        n => format!(", used in {n} past queries"),
    };
    match (&j.documented, j.status) {
        (Some(d), _) if learn::documented(j) => format!("documented ({}){used}", d.source),
        (_, databrain_workspace::JoinStatus::Confirmed) => "confirmed by the user".into(),
        _ if j.queries >= 2 => format!("learned from {} past queries", j.queries),
        _ => format!("learned from a past query run on {} days", j.days),
    }
}

/// `JOIN (cols) = other(cols)  -- learned …` lines for `table`.
pub fn join_lines(table: &str, joins: &[LearnedJoin]) -> Vec<String> {
    joins
        .iter()
        .filter_map(|j| {
            let sd = side(j, table)?;
            let usual = sd.kinds.iter().max_by_key(|(_, n)| *n).map(|(k, _)| k.as_str());
            let usual = match usual {
                Some(k @ ("left" | "right" | "full")) => format!(", usually {} JOIN", k.to_uppercase()),
                _ => String::new(),
            };
            Some(format!("  JOIN ({}) = {}({})  -- {}{usual}", sd.columns.join(", "), sd.other, sd.other_columns.join(", "), evidence(j)))
        })
        .collect()
}

/// One step of a join path: `from(from_columns) = to(to_columns)`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathStep {
    pub from: String,
    pub from_columns: Vec<String>,
    pub to: String,
    pub to_columns: Vec<String>,
    /// `foreign key`, `confirmed by the user` or `learned from …`.
    pub source: String,
}

/// Why a table the AI named can't be used: the same words whether it
/// exists outside the AI's scope or not at all.
pub fn not_available(t: &str) -> String {
    format!(
        "table `{t}` is not available to the AI on this connection; search_schema with other words, or index_tables to ask the user to allow it (don't ask for its columns)"
    )
}

/// Most expansions a path search makes (wide schemas stay bounded).
const PATH_SEARCH_LIMIT: usize = 50_000;

/// The shortest ways (up to 3, at most `max_hops` joins) to join `from` to
/// `to`, over declared foreign keys and [`usable_joins`]. Tables the AI may
/// not read are never part of a path; naming one is refused.
pub fn join_paths(ws: &Workspace, profile: &ConnectionProfile, from: &str, to: &str, max_hops: usize) -> Result<Vec<Vec<PathStep>>> {
    let tables = AiTables::with_profile(ws, profile);
    let find = |t: &str| -> Result<String> {
        let o = tables.get(t)?.ok_or_else(|| AiError::Policy(not_available(t)))?;
        Ok(o.full_name())
    };
    let (from, to) = (find(from)?, find(to)?);
    if from.eq_ignore_ascii_case(&to) {
        return Err(AiError::Policy("`from` and `to` are the same table".into()));
    }
    let readable = |t: &str| tables.readable(t);
    // Undirected edges, each step as seen from the table it leaves.
    let mut adj: HashMap<String, Vec<PathStep>> = HashMap::new();
    let mut add = |a: &str, ac: &[String], b: &str, bc: &[String], source: &str| {
        if !readable(a) || !readable(b) {
            return;
        }
        for (x, xc, y, yc) in [(a, ac, b, bc), (b, bc, a, ac)] {
            adj.entry(x.to_lowercase()).or_default().push(PathStep {
                from: x.to_string(),
                from_columns: xc.to_vec(),
                to: y.to_string(),
                to_columns: yc.to_vec(),
                source: source.to_string(),
            });
        }
    };
    for o in tables.objects()? {
        for fk in &o.foreign_keys {
            let target = tables.get(&format!("{}.{}", fk.ref_schema, fk.ref_table))?.map(|r| r.full_name());
            if let Some(t) = target {
                add(&o.full_name(), &fk.columns, &t, &fk.ref_columns, "foreign key");
            }
        }
    }
    for j in usable_joins(ws, profile)? {
        add(&j.left_table, &j.left_columns, &j.right_table, &j.right_columns, &evidence(&j));
    }
    // Breadth first over paths: every shortest path, without revisiting a table.
    let goal = to.to_lowercase();
    let mut level: Vec<(String, Vec<PathStep>)> = vec![(from.to_lowercase(), vec![])];
    let mut found: Vec<Vec<PathStep>> = Vec::new();
    let mut expansions = 0;
    for _ in 0..max_hops {
        let mut next = Vec::new();
        for (node, path) in &level {
            for step in adj.get(node).into_iter().flatten() {
                expansions += 1;
                let t = step.to.to_lowercase();
                if t == from.to_lowercase() || path.iter().any(|s| s.to.eq_ignore_ascii_case(&t)) {
                    continue;
                }
                let mut p = path.clone();
                p.push(step.clone());
                if t == goal {
                    if !found.contains(&p) {
                        found.push(p);
                    }
                } else {
                    next.push((t, p));
                }
            }
            if expansions > PATH_SEARCH_LIMIT {
                break;
            }
        }
        if !found.is_empty() || next.is_empty() || expansions > PATH_SEARCH_LIMIT {
            break;
        }
        level = next;
    }
    // Declared keys first, then the most used.
    found.sort_by_key(|p| p.iter().filter(|s| s.source != "foreign key").count());
    found.truncate(3);
    Ok(found)
}

/// Check a note target against the connection: every table (and column)
/// must exist. Separate tables with `&`, `,`, `and` or `or`. Uses the
/// knowledge index first, then the live connection. Returns the target with
/// canonical names (`sales.orders & crm.customers.customer_id`), or why not.
pub async fn check_note_target(engine: &QueryEngine, connection_id: &str, target: &str) -> std::result::Result<String, String> {
    let parts = split_target(target);
    if parts.is_empty() {
        return Err("no table given".into());
    }
    if parts.len() > 20 {
        return Err("at most 20 tables per note".into());
    }
    let ws = engine.workspace();
    let mut out = Vec::new();
    let mut missing = Vec::new();
    let mut offline: Option<String> = None;
    for (sep, path) in parts {
        // Tables DataBrain knows (explorer, ⌘P, completion, index runs).
        if let Ok(Some(p)) = ws.meta_resolve_path(connection_id, &path) {
            out.push((sep, p));
            continue;
        }
        match live_limited(resolve_live(engine, connection_id, &path)).await {
            Ok(Some(p)) => out.push((sep, p)),
            Ok(None) => missing.push(path),
            Err(e) => {
                offline.get_or_insert(e);
                missing.push(path);
            }
        }
    }
    if missing.is_empty() {
        return Ok(join_target(&out));
    }
    let list = missing.join(", ");
    Err(match offline {
        Some(e) => format!("cannot check {list}: not in the knowledge index or the tables seen in the explorer, and the connection did not answer ({e})"),
        None => format!("not found in this connection: {list}"),
    })
}

/// Resolve a table reference as written in SQL (`orders`, `sales.orders`,
/// `main.sales.orders`) to (schema id, table name): knowledge index first,
/// then the live connection.
pub async fn resolve_table(engine: &QueryEngine, connection_id: &str, path: &str) -> std::result::Result<Option<(String, String)>, String> {
    if let Ok(Some(hit)) = engine.workspace().meta_get(connection_id, path) {
        return Ok(Some(hit));
    }
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    let Some(table) = parts.last() else { return Ok(None) };
    let schema_written = parts[..parts.len() - 1].join(".").to_ascii_lowercase();
    let hits = engine.search_objects(connection_id, table, 50).await.map_err(|e| e.message)?;
    Ok(hits
        .into_iter()
        .filter(|o| o.kind.is_relation() && o.name.eq_ignore_ascii_case(table))
        .find(|o| {
            let s = o.schema.to_ascii_lowercase();
            schema_written.is_empty() || s == schema_written || s.ends_with(&format!(".{schema_written}"))
        })
        .map(|o| (o.schema, o.name)))
}

/// Live lookups share the connection's metadata session with indexing, so
/// they can wait behind an indexing batch: give up after this long.
const LIVE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(if cfg!(test) { 1 } else { 10 });

async fn live_limited<T>(f: impl std::future::Future<Output = std::result::Result<T, String>>) -> std::result::Result<T, String> {
    tokio::time::timeout(LIVE_CHECK_TIMEOUT, f)
        .await
        .unwrap_or_else(|_| Err(format!("no answer in {} s; the connection may be busy indexing, try again when it finishes", LIVE_CHECK_TIMEOUT.as_secs())))
}

/// `path` = `[catalog.]schema.table[.column]` or a bare `table`, looked up live.
async fn resolve_live(engine: &QueryEngine, connection_id: &str, path: &str) -> std::result::Result<Option<String>, String> {
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    // Splits where the table is part k and at most one column follows.
    for k in (0..parts.len()).rev() {
        let rest = &parts[k + 1..];
        if rest.len() > 1 {
            break;
        }
        let table = parts[k];
        let schema_written = parts[..k].join(".").to_ascii_lowercase();
        let hits = engine.search_objects(connection_id, table, 50).await.map_err(|e| e.message)?;
        let hit = hits.iter().filter(|o| o.kind.is_relation() && o.name.eq_ignore_ascii_case(table)).find(|o| {
            let s = o.schema.to_ascii_lowercase();
            schema_written.is_empty() || s == schema_written || s.ends_with(&format!(".{schema_written}"))
        });
        let Some(o) = hit else { continue };
        let full = format!("{}.{}", o.schema, o.name);
        let Some(col) = rest.first() else { return Ok(Some(full)) };
        let d = engine.describe(connection_id, &o.schema, &o.name).await.map_err(|e| e.message)?;
        if let Some(c) = d.columns.iter().find(|c| c.name.eq_ignore_ascii_case(col)) {
            return Ok(Some(format!("{full}.{}", c.name)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live check stuck behind indexing gives up with a clear error.
    #[tokio::test]
    async fn live_checks_time_out() {
        let t = std::time::Instant::now();
        let e = live_limited(std::future::pending::<std::result::Result<(), String>>()).await.unwrap_err();
        assert!(e.contains("busy indexing"), "{e}");
        assert!(t.elapsed() < std::time::Duration::from_secs(3));
    }
    use databrain_connector_core::{ColumnInfo, ForeignKey};

    #[test]
    fn renders_compact_ddl() {
        let o = KnObject {
            schema: "public".into(),
            name: "orders".into(),
            kind: "table".into(),
            comment: Some("Web orders".into()),
            row_estimate: Some(1200),
            columns: vec![
                ColumnInfo { name: "id".into(), data_type: "int".into(), nullable: false, is_primary_key: true, default: None, comment: None },
                ColumnInfo {
                    name: "email".into(),
                    data_type: "text".into(),
                    nullable: true,
                    is_primary_key: false,
                    default: None,
                    comment: Some("buyer".into()),
                },
            ],
            foreign_keys: vec![ForeignKey {
                columns: vec!["user_id".into()],
                ref_schema: "public".into(),
                ref_table: "users".into(),
                ref_columns: vec!["id".into()],
            }],
        };
        let s = render_object(&o, &|c| c == "email", &["Cancelled orders have status = 'X'".into()]);
        assert!(s.contains("-- table public.orders (~1200 rows): Web orders"));
        assert!(s.contains("id int PK NOT NULL"));
        assert!(s.contains("email text [PII: do not select] -- buyer"));
        assert!(s.contains("FK (user_id) -> public.users(id)"));
        assert!(s.contains("NOTE: Cancelled orders"));
        assert!(system_schema("information_schema") && system_schema("main.INFORMATION_SCHEMA") && !system_schema("public"));
    }

    // ------------------------------------------------------ learned joins

    mod learned {
        use super::super::*;
        use databrain_auth::AuthMethod;
        use databrain_connector_core::{ColumnInfo, ConnectionConfig, ConnectorKind, ForeignKey};
        use databrain_workspace::{AiPolicy, EnvTag, JoinSighting, JoinStatus, Origin};

        const DAY: i64 = 86_400_000;

        fn obj(schema: &str, name: &str, cols: &[&str], fks: Vec<ForeignKey>) -> KnObject {
            KnObject {
                schema: schema.into(),
                name: name.into(),
                kind: "table".into(),
                comment: None,
                row_estimate: None,
                columns: cols
                    .iter()
                    .map(|c| ColumnInfo { name: c.to_string(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None })
                    .collect(),
                foreign_keys: fks,
            }
        }

        /// orders(customer_ref) ~ customers(cust_no) learned; order_items
        /// has a declared FK to orders; regions ~ customers learned once.
        fn setup(policy: AiPolicy) -> (Workspace, ConnectionProfile) {
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
                    ai_policy: policy,
                    created_at: 0,
                    updated_at: 0,
                })
                .unwrap();
            let fk = ForeignKey { columns: vec!["order_id".into()], ref_schema: "sales".into(), ref_table: "orders".into(), ref_columns: vec!["id".into()] };
            ws.kn_upsert_objects(
                &p.id,
                &[
                    obj("sales", "orders", &["id", "customer_ref", "status"], vec![]),
                    obj("sales", "customers", &["id", "cust_no", "region_code", "name"], vec![]),
                    obj("sales", "order_items", &["order_id", "sku"], vec![fk]),
                    obj("ref", "regions", &["code", "label"], vec![]),
                    obj("hr", "salaries", &["cust_no", "amount"], vec![]),
                ],
            )
            .unwrap();
            let now = now_ms();
            let see = |l: (&str, &str), r: (&str, &str), kind: &str, fp: &str, at: i64| {
                ws.record_join(
                    &p.id,
                    &JoinSighting {
                        left_table: l.0.into(),
                        left_columns: vec![l.1.into()],
                        right_table: r.0.into(),
                        right_columns: vec![r.1.into()],
                        kind: kind.into(),
                        declared: false,
                        fingerprint: fp.into(),
                        history_id: 1,
                        at,
                        origin: Origin::User,
                    },
                )
                .unwrap()
            };
            // Two queries: surfaced. Mostly `orders LEFT JOIN customers`.
            see(("sales.orders", "customer_ref"), ("sales.customers", "cust_no"), "left", "q1", now);
            see(("sales.orders", "customer_ref"), ("sales.customers", "cust_no"), "left", "q2", now);
            see(("sales.orders", "customer_ref"), ("sales.customers", "cust_no"), "inner", "q3", now - DAY);
            // Two queries: surfaced.
            see(("sales.customers", "region_code"), ("ref.regions", "code"), "inner", "q4", now);
            see(("sales.customers", "region_code"), ("ref.regions", "code"), "inner", "q5", now);
            // Once: not surfaced.
            see(("sales.customers", "cust_no"), ("hr.salaries", "cust_no"), "inner", "q6", now);
            ws.record_table_use(&p.id, "sales.orders", "q1", now, Origin::User).unwrap();
            (ws, p)
        }

        fn lines(ws: &Workspace, p: &ConnectionProfile, table: &str) -> Vec<String> {
            join_lines(table, &usable_joins(ws, p).unwrap())
        }

        #[test]
        fn learned_joins_render_from_each_side() {
            let (ws, p) = setup(AiPolicy::default());
            assert_eq!(lines(&ws, &p, "sales.orders"), ["  JOIN (customer_ref) = sales.customers(cust_no)  -- learned from 3 past queries, usually LEFT JOIN"]);
            let c = lines(&ws, &p, "sales.customers");
            assert!(c.contains(&"  JOIN (cust_no) = sales.orders(customer_ref)  -- learned from 3 past queries, usually RIGHT JOIN".to_string()), "{c:?}");
            assert!(c.contains(&"  JOIN (region_code) = ref.regions(code)  -- learned from 2 past queries".to_string()), "{c:?}");
            // Seen once: not given to the AI.
            assert!(!c.iter().any(|l| l.contains("hr.salaries")), "{c:?}");
        }

        #[test]
        fn retrieval_adds_learned_partners_and_lines() {
            let (ws, p) = setup(AiPolicy::default());
            let r = retrieve(&ws, &p.id, "orders status", 1, 20_000).unwrap();
            assert_eq!(r.tables[0], "sales.orders");
            assert!(r.tables.contains(&"sales.customers".to_string()), "{:?}", r.tables);
            assert!(r.text.contains("JOIN (customer_ref) = sales.customers(cust_no)"), "{}", r.text);
            assert!(r.learned_joins >= 2);
        }

        #[test]
        fn nothing_learned_is_shown_without_metadata_sharing_or_with_learning_off() {
            for policy in [AiPolicy { share_metadata: false, ..Default::default() }, AiPolicy { learn_from_history: false, ..Default::default() }] {
                let (ws, p) = setup(policy);
                assert!(usable_joins(&ws, &p).unwrap().is_empty());
                let r = retrieve(&ws, &p.id, "orders status", 1, 20_000).unwrap();
                assert!(!r.text.contains("JOIN ("), "{}", r.text);
                assert_eq!(r.learned_joins, 0);
                assert!(!r.tables.contains(&"sales.customers".to_string()), "{:?}", r.tables);
            }
        }

        #[test]
        fn joins_touching_a_table_the_ai_may_not_read_are_hidden() {
            let (ws, p) = setup(AiPolicy { read_deny: vec!["sales.customers".into()], ..Default::default() });
            assert!(usable_joins(&ws, &p).unwrap().is_empty());
            let r = retrieve(&ws, &p.id, "orders status", 1, 20_000).unwrap();
            assert!(!r.text.contains("JOIN ("), "{}", r.text);
            let (ws, p) = setup(AiPolicy { read_allow: vec!["sales.%".into()], ..Default::default() });
            let u = usable_joins(&ws, &p).unwrap();
            assert_eq!(u.len(), 1, "regions is outside the allow list");
            assert_eq!(u[0].right_table, "sales.orders");
        }

        #[test]
        fn rejected_stale_and_declared_joins_are_hidden_and_confirmed_ones_shown() {
            let (ws, p) = setup(AiPolicy::default());
            let all = ws.learned_joins(&p.id).unwrap();
            let id = |t: &str| all.iter().find(|j| j.left_table == t || j.right_table == t).unwrap().id;
            ws.set_join_status(id("ref.regions"), JoinStatus::Rejected).unwrap();
            ws.set_join_status(id("hr.salaries"), JoinStatus::Confirmed).unwrap();
            let c = lines(&ws, &p, "sales.customers");
            assert!(!c.iter().any(|l| l.contains("ref.regions")), "{c:?}");
            assert!(c.iter().any(|l| l.contains("hr.salaries(cust_no)  -- confirmed by the user")), "{c:?}");
            // A column that no longer exists after re-indexing.
            ws.kn_upsert_objects(&p.id, &[obj("sales", "orders", &["id", "status"], vec![])]).unwrap();
            assert!(lines(&ws, &p, "sales.orders").is_empty());
            // Declared: shown as FK, not again as a learned join.
            let mut s = ws.learned_joins(&p.id).unwrap()[0].clone();
            s.declared = true;
            ws.record_join(
                &p.id,
                &JoinSighting {
                    left_table: s.left_table.clone(),
                    left_columns: s.left_columns.clone(),
                    right_table: s.right_table.clone(),
                    right_columns: s.right_columns.clone(),
                    kind: "inner".into(),
                    declared: true,
                    fingerprint: "q9".into(),
                    history_id: 2,
                    at: now_ms(),
                    origin: Origin::User,
                },
            )
            .unwrap();
            assert!(!usable_joins(&ws, &p).unwrap().iter().any(|j| j.id == s.id));
        }

        #[test]
        fn used_tables_rank_higher() {
            let (ws, p) = setup(AiPolicy::default());
            ws.kn_upsert_objects(&p.id, &[obj("sales", "orders_2019", &["id", "status"], vec![])]).unwrap();
            for i in 0..5 {
                ws.record_table_use(&p.id, "sales.orders_2019", &format!("f{i}"), now_ms(), Origin::User).unwrap();
            }
            let r = retrieve(&ws, &p.id, "status", 2, 20_000).unwrap();
            assert_eq!(r.tables[0], "sales.orders_2019", "{:?}", r.tables);
        }

        fn path_text(paths: &[Vec<PathStep>]) -> Vec<String> {
            paths
                .iter()
                .map(|p| {
                    p.iter()
                        .map(|s| format!("{}({}) = {}({}) [{}]", s.from, s.from_columns.join(","), s.to, s.to_columns.join(","), s.source))
                        .collect::<Vec<_>>()
                        .join(" / ")
                })
                .collect()
        }

        #[test]
        fn join_paths_over_foreign_keys_and_learned_joins() {
            let (ws, p) = setup(AiPolicy::default());
            let paths = join_paths(&ws, &p, "order_items", "regions", 3).unwrap();
            assert_eq!(
                path_text(&paths),
                [
                    "sales.order_items(order_id) = sales.orders(id) [foreign key] / sales.orders(customer_ref) = sales.customers(cust_no) [learned from 3 past queries] / sales.customers(region_code) = ref.regions(code) [learned from 2 past queries]"
                ]
            );
            // Too far for two joins.
            assert!(join_paths(&ws, &p, "order_items", "regions", 2).unwrap().is_empty());
            // Not surfaced: no path to salaries.
            assert!(join_paths(&ws, &p, "customers", "hr.salaries", 3).unwrap().is_empty());
            assert!(join_paths(&ws, &p, "orders", "orders", 3).is_err());
            assert!(join_paths(&ws, &p, "orders", "nope", 3).is_err());
        }

        #[test]
        fn join_paths_never_go_through_or_to_a_denied_table() {
            let (ws, p) = setup(AiPolicy { read_deny: vec!["sales.customers".into()], ..Default::default() });
            assert!(join_paths(&ws, &p, "order_items", "regions", 4).unwrap().is_empty());
            let e = join_paths(&ws, &p, "orders", "customers", 3).unwrap_err().to_string();
            // Denied reads as missing: the same words as a table that isn't there.
            assert!(e.contains("not available to the AI"), "{e}");
            assert_eq!(e.replace("customers", "x"), join_paths(&ws, &p, "orders", "x", 3).unwrap_err().to_string());
            // Without learning, declared keys still connect.
            let (ws, p) = setup(AiPolicy { learn_from_history: false, ..Default::default() });
            assert_eq!(join_paths(&ws, &p, "order_items", "orders", 3).unwrap().len(), 1);
            assert!(join_paths(&ws, &p, "order_items", "customers", 3).unwrap().is_empty());
        }
    }
}
