//! Comparing two schemas (same or different connections): which tables,
//! views and routines exist on one side only, and for tables in both, the
//! columns added, removed or changed (type, NULL, default), the primary key
//! and the foreign keys. A script turns the target into the source; it is
//! shown for review and never run here. Statements that lose data (DROP)
//! are written as comments.

use std::collections::BTreeMap;

use databrain_connector_core::{quote_ident, ColumnInfo, ConnectorKind, ForeignKey, ObjectKind, SchemaMetadata, TableColumns};
use serde::{Deserialize, Serialize};

use crate::{EngineError, QueryEngine, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSide {
    pub connection_id: String,
    pub schema: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    /// In the source only (the script creates it).
    OnlySource,
    /// In the target only.
    OnlyTarget,
    Changed,
    Same,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ColumnChange {
    /// Column (None: table-level, e.g. the primary key or a foreign key).
    pub column: Option<String>,
    /// `added` (in the source only), `removed` (in the target only), `type`,
    /// `nullable`, `default`, `primary_key`, `foreign_key`.
    pub what: String,
    pub source: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObjectDiff {
    pub name: String,
    pub kind: ObjectKind,
    pub status: DiffStatus,
    pub changes: Vec<ColumnChange>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaDiff {
    pub source_kind: ConnectorKind,
    pub target_kind: ConnectorKind,
    pub objects: Vec<ObjectDiff>,
    /// Statements that make the target like the source (target dialect).
    pub script: String,
}

fn norm_type(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase()
}

fn norm_default(d: &Option<String>) -> Option<String> {
    d.as_ref().map(|s| s.trim().trim_start_matches('(').trim_end_matches(')').to_ascii_lowercase()).filter(|s| !s.is_empty() && s != "null")
}

fn key(s: &str) -> String {
    s.to_ascii_lowercase()
}

fn fk_text(fk: &ForeignKey) -> String {
    format!("({}) → {}({})", fk.columns.join(", "), fk.ref_table, fk.ref_columns.join(", "))
}

fn fk_same(a: &ForeignKey, b: &ForeignKey) -> bool {
    let l = |v: &[String]| v.iter().map(|x| key(x)).collect::<Vec<_>>();
    l(&a.columns) == l(&b.columns) && key(&a.ref_table) == key(&b.ref_table) && l(&a.ref_columns) == l(&b.ref_columns)
}

/// Column changes between a source and a target table.
pub fn table_changes(src: &TableColumns, dst: &TableColumns) -> Vec<ColumnChange> {
    let mut out = Vec::new();
    let find = |cols: &[ColumnInfo], n: &str| cols.iter().find(|c| c.name.eq_ignore_ascii_case(n)).cloned();
    for c in &src.columns {
        match find(&dst.columns, &c.name) {
            None => out.push(ColumnChange { column: Some(c.name.clone()), what: "added".into(), source: Some(c.data_type.clone()), target: None }),
            Some(d) => {
                if norm_type(&c.data_type) != norm_type(&d.data_type) {
                    out.push(ColumnChange { column: Some(c.name.clone()), what: "type".into(), source: Some(c.data_type.clone()), target: Some(d.data_type.clone()) });
                }
                if c.nullable != d.nullable {
                    let t = |n: bool| Some(if n { "NULL" } else { "NOT NULL" }.to_string());
                    out.push(ColumnChange { column: Some(c.name.clone()), what: "nullable".into(), source: t(c.nullable), target: t(d.nullable) });
                }
                if norm_default(&c.default) != norm_default(&d.default) {
                    out.push(ColumnChange { column: Some(c.name.clone()), what: "default".into(), source: c.default.clone(), target: d.default.clone() });
                }
            }
        }
    }
    for d in &dst.columns {
        if find(&src.columns, &d.name).is_none() {
            out.push(ColumnChange { column: Some(d.name.clone()), what: "removed".into(), source: None, target: Some(d.data_type.clone()) });
        }
    }
    let pk = |t: &TableColumns| t.columns.iter().filter(|c| c.is_primary_key).map(|c| key(&c.name)).collect::<Vec<_>>();
    if pk(src) != pk(dst) {
        let show = |t: &TableColumns| Some(t.columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty());
        out.push(ColumnChange { column: None, what: "primary_key".into(), source: show(src), target: show(dst) });
    }
    for fk in &src.foreign_keys {
        if !dst.foreign_keys.iter().any(|x| fk_same(fk, x)) {
            out.push(ColumnChange { column: None, what: "foreign_key".into(), source: Some(fk_text(fk)), target: None });
        }
    }
    for fk in &dst.foreign_keys {
        if !src.foreign_keys.iter().any(|x| fk_same(fk, x)) {
            out.push(ColumnChange { column: None, what: "foreign_key".into(), source: None, target: Some(fk_text(fk)) });
        }
    }
    out
}

pub fn diff_metadata(src: &SchemaMetadata, dst: &SchemaMetadata) -> Vec<ObjectDiff> {
    let cols = |m: &SchemaMetadata| m.columns.iter().map(|t| (key(&t.table), t.clone())).collect::<BTreeMap<_, _>>();
    let (sc, dc) = (cols(src), cols(dst));
    let objs = |m: &SchemaMetadata| m.objects.iter().map(|o| (key(&o.name), o.clone())).collect::<BTreeMap<_, _>>();
    let (so, dobj) = (objs(src), objs(dst));
    let mut out = Vec::new();
    let names: std::collections::BTreeSet<&String> = so.keys().chain(dobj.keys()).collect();
    for k in names {
        let (s, d) = (so.get(k), dobj.get(k));
        let kind = s.or(d).map(|o| o.kind).unwrap_or(ObjectKind::Table);
        let name = s.or(d).map(|o| o.name.clone()).unwrap_or_default();
        let status = match (s, d) {
            (Some(_), None) => DiffStatus::OnlySource,
            (None, Some(_)) => DiffStatus::OnlyTarget,
            _ => DiffStatus::Same,
        };
        let mut diff = ObjectDiff { name, kind, status, changes: vec![] };
        if status == DiffStatus::Same && kind.is_relation() {
            if let (Some(a), Some(b)) = (sc.get(k), dc.get(k)) {
                diff.changes = table_changes(a, b);
                if !diff.changes.is_empty() {
                    diff.status = DiffStatus::Changed;
                }
            }
        }
        out.push(diff);
    }
    out
}

fn col_def(kind: ConnectorKind, c: &ColumnInfo) -> String {
    let mut s = format!("{} {}", quote_ident(kind, &c.name), c.data_type);
    if let Some(d) = c.default.as_ref().filter(|d| !d.trim().is_empty()) {
        s.push_str(&format!(" DEFAULT {d}"));
    }
    if !c.nullable {
        s.push_str(" NOT NULL");
    }
    s
}

/// The script that makes the target schema like the source (target dialect).
pub fn migration_script(kind: ConnectorKind, target_schema: &str, source_schema: &str, src: &SchemaMetadata, diffs: &[ObjectDiff], cross_engine: bool) -> String {
    use ConnectorKind::*;
    let table = |n: &str| format!("{}.{}", target_schema.split('.').map(|p| quote_ident(kind, p)).collect::<Vec<_>>().join("."), quote_ident(kind, n));
    // SQLite doesn't take a schema in REFERENCES (it is the table's own).
    let ref_table = |n: &str| if kind == Sqlite { quote_ident(kind, n) } else { table(n) };
    let src_cols = |n: &str| src.columns.iter().find(|t| t.table.eq_ignore_ascii_case(n));
    let mut out = vec![format!("-- Make {target_schema} like {source_schema}. Review before running: nothing has run.")];
    if cross_engine {
        out.push("-- The schemas are on different database types: column types are copied as written in the source.".into());
    }
    for d in diffs {
        let t = table(&d.name);
        match d.status {
            DiffStatus::Same => {}
            DiffStatus::OnlySource if d.kind == ObjectKind::Table => {
                let Some(tc) = src_cols(&d.name) else { continue };
                let mut lines: Vec<String> = tc.columns.iter().map(|c| format!("  {}", col_def(kind, c))).collect();
                let pk: Vec<String> = tc.columns.iter().filter(|c| c.is_primary_key).map(|c| quote_ident(kind, &c.name)).collect();
                if !pk.is_empty() {
                    lines.push(format!("  PRIMARY KEY ({})", pk.join(", ")));
                }
                for fk in &tc.foreign_keys {
                    let cols = fk.columns.iter().map(|x| quote_ident(kind, x)).collect::<Vec<_>>().join(", ");
                    let refs = fk.ref_columns.iter().map(|x| quote_ident(kind, x)).collect::<Vec<_>>().join(", ");
                    lines.push(format!("  FOREIGN KEY ({cols}) REFERENCES {} ({refs})", ref_table(&fk.ref_table)));
                }
                out.push(format!("\nCREATE TABLE {t} (\n{}\n);", lines.join(",\n")));
            }
            DiffStatus::OnlySource => out.push(format!("\n-- {} {} exists only in the source: copy its DDL (Show DDL on it).", d.kind.as_str().replace('_', " "), d.name)),
            DiffStatus::OnlyTarget => {
                if d.kind == ObjectKind::Table {
                    out.push(format!("\n-- {} is not in the source (drops its data):\n-- DROP TABLE {t};", d.name));
                } else {
                    out.push(format!("\n-- {} {} is not in the source.", d.kind.as_str().replace('_', " "), d.name));
                }
            }
            DiffStatus::Changed => {
                out.push(format!("\n-- {}", d.name));
                let tc = src_cols(&d.name);
                for ch in &d.changes {
                    let col = ch.column.as_deref().unwrap_or("");
                    let c = quote_ident(kind, col);
                    let info = tc.and_then(|tc| tc.columns.iter().find(|x| x.name.eq_ignore_ascii_case(col)));
                    match (ch.what.as_str(), info) {
                        ("added", Some(info)) => {
                            // NOT NULL without a default fails on a table with rows.
                            let mut def = info.clone();
                            let note = if !def.nullable && def.default.is_none() {
                                def.nullable = true;
                                " -- NOT NULL in the source: fill the column, then set NOT NULL"
                            } else {
                                ""
                            };
                            let d = col_def(kind, &def);
                            out.push(match kind {
                                Mssql => format!("ALTER TABLE {t} ADD {d};{note}"),
                                Oracle => format!("ALTER TABLE {t} ADD ({d});{note}"),
                                _ => format!("ALTER TABLE {t} ADD COLUMN {d};{note}"),
                            });
                        }
                        ("removed", _) => out.push(format!("-- ALTER TABLE {t} DROP COLUMN {c};  (drops its data)")),
                        ("type" | "nullable", Some(info)) => {
                            let ty = &info.data_type;
                            let null = if info.nullable { "NULL" } else { "NOT NULL" };
                            out.push(match kind {
                                Postgres if ch.what == "type" => format!("ALTER TABLE {t} ALTER COLUMN {c} TYPE {ty};"),
                                Postgres | Duckdb | Snowflake if ch.what == "nullable" => format!("ALTER TABLE {t} ALTER COLUMN {c} {} NOT NULL;", if info.nullable { "DROP" } else { "SET" }),
                                Duckdb => format!("ALTER TABLE {t} ALTER {c} TYPE {ty};"),
                                Snowflake => format!("ALTER TABLE {t} ALTER COLUMN {c} SET DATA TYPE {ty};"),
                                Mysql => format!("ALTER TABLE {t} MODIFY COLUMN {};", col_def(kind, info)),
                                Mssql => format!("ALTER TABLE {t} ALTER COLUMN {c} {ty} {null};"),
                                Oracle if ch.what == "type" => format!("ALTER TABLE {t} MODIFY ({c} {ty});"),
                                Oracle => format!("ALTER TABLE {t} MODIFY ({c} {null});"),
                                Databricks if ch.what == "type" => format!("ALTER TABLE {t} ALTER COLUMN {c} TYPE {ty};"),
                                Databricks => format!("ALTER TABLE {t} ALTER COLUMN {c} {} NOT NULL;", if info.nullable { "DROP" } else { "SET" }),
                                _ => format!("-- {col}: {} → {}: this database can't change a column in place (rebuild the table).", ch.target.clone().unwrap_or_default(), ch.source.clone().unwrap_or_default()),
                            });
                        }
                        ("default", Some(info)) => out.push(match (kind, &info.default) {
                            (Mysql, _) => format!("ALTER TABLE {t} MODIFY COLUMN {};", col_def(kind, info)),
                            (Mssql | Sqlite, _) => format!("-- {col}: default {} → {} (change the default constraint by hand)", ch.target.clone().unwrap_or("none".into()), ch.source.clone().unwrap_or("none".into())),
                            (Oracle, Some(d)) => format!("ALTER TABLE {t} MODIFY ({c} DEFAULT {d});"),
                            (Oracle, None) => format!("ALTER TABLE {t} MODIFY ({c} DEFAULT NULL);"),
                            (_, Some(d)) => format!("ALTER TABLE {t} ALTER COLUMN {c} SET DEFAULT {d};"),
                            (_, None) => format!("ALTER TABLE {t} ALTER COLUMN {c} DROP DEFAULT;"),
                        }),
                        ("primary_key", _) => out.push(format!("-- primary key: target ({}) → source ({})", ch.target.clone().unwrap_or("none".into()), ch.source.clone().unwrap_or("none".into()))),
                        ("foreign_key", _) if ch.source.is_some() => {
                            let fk = tc.and_then(|tc| tc.foreign_keys.iter().find(|f| Some(fk_text(f)) == ch.source));
                            match (fk, kind) {
                                (Some(fk), k) if k != Sqlite => {
                                    let cols = fk.columns.iter().map(|x| quote_ident(kind, x)).collect::<Vec<_>>().join(", ");
                                    let refs = fk.ref_columns.iter().map(|x| quote_ident(kind, x)).collect::<Vec<_>>().join(", ");
                                    out.push(format!("ALTER TABLE {t} ADD FOREIGN KEY ({cols}) REFERENCES {} ({refs});", ref_table(&fk.ref_table)));
                                }
                                _ => out.push(format!("-- foreign key {} (add it by hand)", ch.source.clone().unwrap_or_default())),
                            }
                        }
                        ("foreign_key", _) => out.push(format!("-- foreign key {} is not in the source", ch.target.clone().unwrap_or_default())),
                        _ => {}
                    }
                }
            }
        }
    }
    if out.len() <= 2 {
        out.push("\n-- Nothing to change.".into());
    }
    out.join("\n") + "\n"
}

impl QueryEngine {
    /// Compare `source` with `target`; the script changes the target.
    pub async fn schema_diff(&self, source: &SchemaSide, target: &SchemaSide) -> Result<SchemaDiff> {
        let sk = self.workspace().get_connection(&source.connection_id)?.config.kind;
        let tk = self.workspace().get_connection(&target.connection_id)?.config.kind;
        let (a, b) = tokio::join!(self.bulk_metadata(&source.connection_id, std::slice::from_ref(&source.schema)), self.bulk_metadata(&target.connection_id, std::slice::from_ref(&target.schema)));
        let one = |r: Result<Vec<SchemaMetadata>>, side: &SchemaSide| -> Result<SchemaMetadata> {
            let m = r?.into_iter().next().ok_or_else(|| EngineError::new("not_found", format!("Schema {} not found", side.schema)))?;
            if let Some(e) = &m.error {
                return Err(EngineError::new("metadata", format!("{}: {e}", side.schema)));
            }
            Ok(m)
        };
        let (a, b) = (one(a, source)?, one(b, target)?);
        let objects = diff_metadata(&a, &b);
        let script = migration_script(tk, &target.schema, &source.schema, &a, &objects, sk != tk);
        Ok(SchemaDiff { source_kind: sk, target_kind: tk, objects, script })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::DbObject;

    fn col(n: &str, t: &str, null: bool, pk: bool) -> ColumnInfo {
        ColumnInfo { name: n.into(), data_type: t.into(), nullable: null, is_primary_key: pk, default: None, comment: None }
    }
    fn meta(tables: Vec<(&str, Vec<ColumnInfo>)>) -> SchemaMetadata {
        SchemaMetadata {
            schema: "s".into(),
            objects: tables.iter().map(|(n, _)| DbObject { schema: "s".into(), name: (*n).into(), kind: ObjectKind::Table, comment: None, row_estimate: None }).collect(),
            columns: tables.into_iter().map(|(n, c)| TableColumns { table: n.into(), columns: c, foreign_keys: vec![] }).collect(),
            error: None,
        }
    }

    #[test]
    fn finds_differences_and_writes_the_script() {
        let src = meta(vec![("orders", vec![col("id", "integer", false, true), col("amount", "numeric(12,2)", false, false), col("note", "text", true, false)]), ("customers", vec![col("id", "integer", false, true)])]);
        let dst = meta(vec![("ORDERS", vec![col("id", "integer", false, true), col("amount", "numeric(10,2)", true, false), col("legacy", "text", true, false)]), ("old_log", vec![col("x", "text", true, false)])]);
        let d = diff_metadata(&src, &dst);
        let by = |n: &str| d.iter().find(|x| x.name.eq_ignore_ascii_case(n)).unwrap();
        assert_eq!(by("customers").status, DiffStatus::OnlySource);
        assert_eq!(by("old_log").status, DiffStatus::OnlyTarget);
        let o = by("orders");
        assert_eq!(o.status, DiffStatus::Changed);
        let whats: Vec<(&str, &str)> = o.changes.iter().map(|c| (c.column.as_deref().unwrap_or(""), c.what.as_str())).collect();
        assert_eq!(whats, [("amount", "type"), ("amount", "nullable"), ("note", "added"), ("legacy", "removed")]);

        let s = migration_script(ConnectorKind::Postgres, "public", "dev", &src, &d, false);
        assert!(s.contains("CREATE TABLE \"public\".\"customers\" (\n  \"id\" integer NOT NULL,\n  PRIMARY KEY (\"id\")\n);"), "{s}");
        assert!(s.contains("ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"amount\" TYPE numeric(12,2);"), "{s}");
        assert!(s.contains("ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"amount\" SET NOT NULL;"), "{s}");
        assert!(s.contains("ALTER TABLE \"public\".\"orders\" ADD COLUMN \"note\" text;"), "{s}");
        assert!(s.contains("-- ALTER TABLE \"public\".\"orders\" DROP COLUMN \"legacy\";"), "drops are comments: {s}");
        assert!(s.contains("-- DROP TABLE \"public\".\"old_log\";"), "{s}");

        let m = migration_script(ConnectorKind::Mssql, "dbo", "dev", &src, &d, false);
        assert!(m.contains("ALTER TABLE \"dbo\".\"orders\" ALTER COLUMN \"amount\" numeric(12,2) NOT NULL;") || m.contains("ALTER TABLE [dbo].[orders] ALTER COLUMN [amount] numeric(12,2) NOT NULL;"), "{m}");
    }
}
