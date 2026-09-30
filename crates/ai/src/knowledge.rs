//! Database metadata as AI knowledge: indexing schemas into the workspace
//! store and retrieving the most relevant tables (BM25 + foreign-key
//! expansion) rendered as compact DDL for prompts.

use std::collections::HashSet;
use std::sync::Arc;

use databrain_query_engine::QueryEngine;
use databrain_workspace::{KnObject, KnState, NoteStatus, Workspace, now_ms};
use serde::Serialize;

use crate::policy::is_pii;
use crate::types::Result;

#[derive(Debug, Clone, Serialize)]
pub struct IndexReport {
    pub schemas: usize,
    pub objects: usize,
    pub changed: usize,
    pub removed: usize,
    pub errors: Vec<String>,
}

/// Progress callback: (schema, done, total).
pub type Progress<'a> = &'a (dyn Fn(&str, usize, usize) + Send + Sync);

fn system_schema(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let last = n.rsplit('.').next().unwrap_or(&n);
    matches!(
        last,
        "information_schema" | "pg_catalog" | "pg_toast" | "sys" | "mysql" | "performance_schema" | "sysibm" | "ctxsys" | "mdsys" | "xdb" | "outln"
    ) || last.starts_with("pg_temp")
}

/// (Re)index a connection's metadata. Honors `ai_policy.index_schemas`.
pub async fn index_connection(engine: &Arc<QueryEngine>, connection_id: &str, progress: Progress<'_>) -> Result<IndexReport> {
    let ws = engine.workspace().clone();
    let profile = ws.get_connection(connection_id)?;
    let wanted = &profile.ai_policy.index_schemas;
    let schemas: Vec<String> = engine
        .list_schemas(connection_id)
        .await?
        .into_iter()
        .map(|s| s.name)
        .filter(|s| if wanted.is_empty() { !system_schema(s) } else { wanted.iter().any(|w| w.eq_ignore_ascii_case(s)) })
        .collect();
    let mut report = IndexReport { schemas: schemas.len(), objects: 0, changed: 0, removed: 0, errors: vec![] };
    for (i, schema) in schemas.iter().enumerate() {
        progress(schema, i, schemas.len());
        let objects = match engine.list_objects(connection_id, schema).await {
            Ok(o) => o,
            Err(e) => {
                report.errors.push(format!("{schema}: {}", e.message));
                continue;
            }
        };
        let cols = match engine.schema_columns(connection_id, schema).await {
            Ok(c) => c,
            Err(e) => {
                report.errors.push(format!("{schema}: {}", e.message));
                vec![]
            }
        };
        let items: Vec<KnObject> = objects
            .into_iter()
            .filter(|o| !matches!(o.kind, databrain_connector_core::ObjectKind::Function | databrain_connector_core::ObjectKind::Procedure | databrain_connector_core::ObjectKind::Sequence))
            .map(|o| {
                let tc = cols.iter().find(|t| t.table == o.name);
                KnObject {
                    schema: schema.clone(),
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
        let d = ws.kn_replace_schema(connection_id, schema, &items)?;
        report.changed += d.changed;
        report.removed += d.removed;
    }
    report.removed += ws.kn_retain_schemas(connection_id, &schemas)?;
    progress("", schemas.len(), schemas.len());
    ws.kn_set_state(&KnState {
        connection_id: connection_id.to_string(),
        indexed_at: now_ms(),
        objects: ws.kn_count(connection_id)?,
        schemas: schemas.clone(),
        error: (!report.errors.is_empty()).then(|| report.errors.join("; ")),
    })?;
    Ok(report)
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
}

/// Retrieve relevant schema context for `question` within ~`budget_chars`.
pub fn retrieve(ws: &Workspace, connection_id: &str, question: &str, max_tables: usize, budget_chars: usize) -> Result<Retrieved> {
    let profile = ws.get_connection(connection_id)?;
    let policy = profile.ai_policy.clone();
    let notes_all = ws.kn_notes(connection_id)?.into_iter().filter(|n| n.status == NoteStatus::Approved).collect::<Vec<_>>();
    let hits = ws.kn_search(connection_id, question, 40)?;
    let mut chosen: Vec<KnObject> = Vec::new();
    let mut seen = HashSet::new();
    let mut glossary: Vec<String> = Vec::new();
    for h in &hits {
        if h.source == "note" {
            if let Some(n) = notes_all.iter().find(|n| n.id == h.reference) {
                if n.target.is_none() {
                    glossary.push(n.body.clone());
                } else if let Some(o) = ws.kn_get(connection_id, n.target.as_deref().unwrap_or(""))? {
                    if seen.insert(o.full_name()) {
                        chosen.push(o);
                    }
                }
            }
            continue;
        }
        if chosen.len() >= max_tables {
            break;
        }
        if let Some(o) = ws.kn_get(connection_id, &h.reference)? {
            if seen.insert(o.full_name()) {
                chosen.push(o);
            }
        }
    }
    // Small databases: include everything when nothing matched.
    if chosen.is_empty() && ws.kn_count(connection_id)? <= max_tables as i64 {
        for o in ws.kn_objects(connection_id)? {
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
            if let Some(r) = ws.kn_get(connection_id, &format!("{}.{}", fk.ref_schema, fk.ref_table))? {
                if seen.insert(r.full_name()) {
                    chosen.push(r);
                }
            }
        }
    }
    let mut text = String::new();
    let mut tables = Vec::new();
    for o in &chosen {
        let full = o.full_name();
        let table_notes: Vec<String> = notes_all
            .iter()
            .filter(|n| n.target.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(&full) || t.eq_ignore_ascii_case(&o.name) || t.to_ascii_lowercase().starts_with(&format!("{}.", full.to_ascii_lowercase()))))
            .map(|n| match n.target.as_deref() {
                Some(t) if !t.eq_ignore_ascii_case(&full) => format!("{t}: {}", n.body),
                _ => n.body.clone(),
            })
            .collect();
        let pii = |c: &str| is_pii(&policy, Some(&full), c);
        let block = if policy.share_metadata { render_object(o, &pii, &table_notes) } else { format!("-- {} {} (metadata sharing disabled)", o.kind, full) };
        if !text.is_empty() && text.len() + block.len() > budget_chars {
            break;
        }
        text.push_str(&block);
        text.push_str("\n\n");
        tables.push(full);
    }
    Ok(Retrieved { tables, text: text.trim_end().to_string(), notes: glossary })
}

#[cfg(test)]
mod tests {
    use super::*;
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
                ColumnInfo { name: "email".into(), data_type: "text".into(), nullable: true, is_primary_key: false, default: None, comment: Some("buyer".into()) },
            ],
            foreign_keys: vec![ForeignKey { columns: vec!["user_id".into()], ref_schema: "public".into(), ref_table: "users".into(), ref_columns: vec!["id".into()] }],
        };
        let s = render_object(&o, &|c| c == "email", &["Cancelled orders have status = 'X'".into()]);
        assert!(s.contains("-- table public.orders (~1200 rows): Web orders"));
        assert!(s.contains("id int PK NOT NULL"));
        assert!(s.contains("email text [PII: do not select] -- buyer"));
        assert!(s.contains("FK (user_id) -> public.users(id)"));
        assert!(s.contains("NOTE: Cancelled orders"));
        assert!(system_schema("information_schema") && system_schema("main.INFORMATION_SCHEMA") && !system_schema("public"));
    }
}
