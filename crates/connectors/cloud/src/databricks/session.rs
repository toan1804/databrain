//! `Session` implementation: execution and catalog browsing.

use async_trait::async_trait;

use serde_json::json;

use databrain_connector_core::{
    ConnectorError, ConnectorKind, CostEstimate, DbObject, ExecOptions, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, SchemaMetadata, Session,
    TableColumns, quote_ident, quote_literal,
};

use super::client::DbxSession;
use super::metadata::{cell, databricks_stat_rows, split_schema};

#[async_trait]
impl Session for DbxSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Databricks
    }

    /// Databricks SQL commits each statement; there are no multi-statement transactions.
    fn transaction_state(&self) -> databrain_connector_core::TxState {
        databrain_connector_core::TxState::None
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.0.run_small("SELECT current_version().dbsql_version").await.unwrap_or_default();
        let v = r.first().and_then(|r| cell(r, 0)).unwrap_or_default();
        Ok(format!("Databricks SQL {v} (warehouse {})", self.0.warehouse).replace("  ", " "))
    }

    async fn ping(&self) -> Result<()> {
        self.0.run_small("SELECT 1").await.map(|_| ())
    }

    async fn execute(&self, stmt: databrain_connector_core::Statement<'_>, opts: ExecOptions) -> Result<QueryStream> {
        let sql = stmt.sql();
        let (tx, stream) = QueryStream::channel(4);
        let inner = self.0.clone();
        let sql = sql.to_string();
        tokio::spawn(async move {
            if let Err(e) = inner.stream(&sql, &opts, &tx).await {
                tx.send_err(e).await;
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let default_cat = self.0.catalog.lock().await.clone();
        let default_sch = self.0.schema.lock().await.clone().unwrap_or_else(|| "default".into());
        let rows = self
            .0
            .run_small(
                "SELECT catalog_name, schema_name FROM system.information_schema.schemata \
                 WHERE schema_name <> 'information_schema' ORDER BY catalog_name, schema_name",
            )
            .await;
        let rows = match rows {
            Ok(r) => r,
            Err(_) => {
                // Workspaces without Unity Catalog: hive_metastore only.
                let r = self.0.run_small("SHOW SCHEMAS IN hive_metastore").await?;
                r.into_iter().map(|x| vec![json!("hive_metastore"), x.first().cloned().unwrap_or_default()]).collect()
            }
        };
        let mut out: Vec<SchemaInfo> = rows
            .iter()
            .filter_map(|r| {
                let (c, s) = (cell(r, 0)?, cell(r, 1)?);
                let is_default = default_cat.as_deref().is_none_or(|d| d == c) && s == default_sch;
                Some(SchemaInfo::in_catalog(c, &s, is_default))
            })
            .collect();
        out.sort_by_key(|s| !s.is_default);
        Ok(out)
    }

    async fn list_catalogs(&self) -> Result<Option<Vec<databrain_connector_core::CatalogInfo>>> {
        let default_cat = self.0.catalog.lock().await.clone();
        let rows = match self.0.run_small("SHOW CATALOGS").await {
            Ok(r) => r,
            Err(_) => vec![vec![json!("hive_metastore")]],
        };
        let mut out: Vec<databrain_connector_core::CatalogInfo> = rows
            .iter()
            .filter_map(|r| cell(r, 0))
            .map(|name| databrain_connector_core::CatalogInfo { is_default: default_cat.as_deref() == Some(name.as_str()), name })
            .collect();
        out.sort_by(|a, b| b.is_default.cmp(&a.is_default).then_with(|| a.name.cmp(&b.name)));
        Ok(Some(out))
    }

    async fn list_catalog_schemas(&self, catalog: &str) -> Result<Vec<SchemaInfo>> {
        let default_cat = self.0.catalog.lock().await.clone();
        let default_sch = self.0.schema.lock().await.clone().unwrap_or_else(|| "default".into());
        let rows = self.0.run_small(&format!("SHOW SCHEMAS IN {}", quote_ident(ConnectorKind::Databricks, catalog))).await?;
        let mut out: Vec<SchemaInfo> = rows
            .iter()
            .filter_map(|r| cell(r, 0))
            .filter(|s| s != "information_schema")
            .map(|s| {
                let is_default = default_cat.as_deref().is_none_or(|d| d == catalog) && s == default_sch;
                SchemaInfo::in_catalog(catalog, &s, is_default)
            })
            .collect();
        out.sort_by(|a, b| b.is_default.cmp(&a.is_default).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    async fn search_objects(&self, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let term = databrain_connector_core::search_sql_term(query);
        let sql = format!(
            "SELECT table_catalog, table_schema, table_name, table_type, comment FROM system.information_schema.tables \
             WHERE table_schema <> 'information_schema' AND instr(lower(table_name), {}) > 0 LIMIT {}",
            quote_literal(&term),
            (limit * 4).max(200)
        );
        let rows = match self.0.run_small(&sql).await {
            Ok(r) => r,
            // No Unity Catalog: walk hive_metastore schema by schema.
            Err(_) => return databrain_connector_core::search_by_listing(self, query, limit).await,
        };
        let mut hits: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                let t = cell(r, 3).unwrap_or_default();
                Some(DbObject {
                    schema: format!("{}.{}", cell(r, 0)?, cell(r, 1)?),
                    name: cell(r, 2)?,
                    kind: if t.contains("VIEW") {
                        if t.contains("MATERIALIZED") { ObjectKind::MaterializedView } else { ObjectKind::View }
                    } else {
                        ObjectKind::Table
                    },
                    comment: cell(r, 4),
                    row_estimate: None,
                })
            })
            .filter(|o| databrain_connector_core::object_matches(query, &o.schema, &o.name))
            .collect();
        databrain_connector_core::rank_matches(query, &mut hits, limit);
        Ok(hits)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let (cat, sch) = split_schema(schema)?;
        let sql = format!(
            "SELECT table_name, table_type, comment FROM {}.information_schema.tables WHERE table_schema = {} ORDER BY table_type, table_name",
            quote_ident(ConnectorKind::Databricks, cat),
            quote_literal(sch)
        );
        let rows = match self.0.run_small(&sql).await {
            Ok(r) => r,
            Err(_) => {
                let r = self.0.run_small(&format!("SHOW TABLES IN {}", databrain_connector_core::quote_path(ConnectorKind::Databricks, schema))).await?;
                r.into_iter().map(|x| vec![x.get(1).cloned().unwrap_or_default(), json!("TABLE"), serde_json::Value::Null]).collect()
            }
        };
        let mut out: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                let name = cell(r, 0)?;
                let t = cell(r, 1).unwrap_or_default();
                Some(DbObject {
                    schema: schema.to_string(),
                    name,
                    kind: if t.contains("VIEW") {
                        if t.contains("MATERIALIZED") { ObjectKind::MaterializedView } else { ObjectKind::View }
                    } else {
                        ObjectKind::Table
                    },
                    comment: cell(r, 2),
                    row_estimate: None,
                })
            })
            .collect();
        // Unity Catalog functions (best effort).
        let q = format!(
            "SELECT routine_name, comment FROM {}.information_schema.routines WHERE routine_schema = {} ORDER BY routine_name",
            quote_ident(ConnectorKind::Databricks, cat),
            quote_literal(sch)
        );
        if let Ok(rows) = self.0.run_small(&q).await {
            out.extend(rows.iter().filter_map(|r| {
                Some(DbObject { schema: schema.to_string(), name: cell(r, 0)?, kind: ObjectKind::Function, comment: cell(r, 1), row_estimate: None })
            }));
        }
        Ok(out)
    }

    async fn search_schema(&self, schema: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let (cat, sch) = split_schema(schema)?;
        let term = databrain_connector_core::search_sql_term(query);
        let t = quote_literal(&term);
        let sql = format!(
            "SELECT table_name, table_type, comment FROM {}.information_schema.tables WHERE table_schema = {} AND instr(lower(table_name), {t}) > 0 \
             ORDER BY CASE WHEN instr(lower(table_name), {t}) = 1 THEN 0 ELSE 1 END, length(table_name), table_name LIMIT {}",
            quote_ident(ConnectorKind::Databricks, cat),
            quote_literal(sch),
            limit.clamp(1, 1000)
        );
        let Ok(rows) = self.0.run_small(&sql).await else {
            // hive_metastore: no information_schema.
            return databrain_connector_core::search_schema_by_listing(self, schema, query, limit).await;
        };
        let mut hits: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                let t = cell(r, 1).unwrap_or_default();
                Some(DbObject {
                    schema: schema.to_string(),
                    name: cell(r, 0)?,
                    kind: if t.contains("VIEW") {
                        if t.contains("MATERIALIZED") { ObjectKind::MaterializedView } else { ObjectKind::View }
                    } else {
                        ObjectKind::Table
                    },
                    comment: cell(r, 2),
                    row_estimate: None,
                })
            })
            .collect();
        databrain_connector_core::rank_matches(query, &mut hits, limit);
        Ok(hits)
    }

    /// `EXPLAIN COST`: the optimized plan with Spark's size and row
    /// estimates from table statistics. Nothing runs, but the warehouse
    /// must be up to answer (a stopped one starts).
    async fn estimate_cost(&self, sql: &databrain_connector_core::Checked) -> Result<Option<CostEstimate>> {
        let sql = sql.sql();
        let rows = databrain_connector_core::query_text(self, &format!("EXPLAIN COST {sql}")).await?;
        let text = rows.iter().filter_map(|r| r.first().cloned().flatten()).collect::<Vec<_>>().join("\n");
        explain_cost_estimate(&text).map(Some)
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        use databrain_connector_core::{TableLayout, parse_list_text, query_table};
        let full = format!("{}.{}", databrain_connector_core::quote_path(ConnectorKind::Databricks, schema), quote_ident(ConnectorKind::Databricks, name));
        let mut l = TableLayout::default();
        // Views and non-Delta tables have no detail: no hints, not an error.
        let Ok((names, rows)) = query_table(self, databrain_connector_core::Statement::internal(&format!("DESCRIBE DETAIL {full}"))).await else {
            return Ok(l);
        };
        let Some(r) = rows.first() else { return Ok(l) };
        let get = |n: &str| names.iter().position(|x| x.eq_ignore_ascii_case(n)).and_then(|i| r.get(i).cloned().flatten());
        l.partition_by = get("partitionColumns").as_deref().map(parse_list_text).unwrap_or_default();
        if !l.partition_by.is_empty() {
            l.partition_kind = Some("identity".into());
        }
        l.cluster_by = get("clusteringColumns").as_deref().map(parse_list_text).unwrap_or_default();
        if let (Some(files), Some(bytes)) = (get("numFiles"), get("sizeInBytes").and_then(|b| b.parse::<f64>().ok())) {
            l.notes.push(format!("{} · {files} files · {:.1} GB", get("format").unwrap_or_else(|| "delta".into()), bytes / 1e9));
        }
        l.size_bytes = get("sizeInBytes").and_then(|b| b.parse().ok());
        // Rows: only from statistics (ANALYZE TABLE … COMPUTE STATISTICS), never a scan.
        if let Ok((names, rows)) = query_table(self, databrain_connector_core::Statement::internal(&format!("DESCRIBE TABLE EXTENDED {full}"))).await {
            let col = |n: &str| names.iter().position(|x| x.eq_ignore_ascii_case(n));
            if let (Some(k), Some(v)) = (col("col_name"), col("data_type")) {
                let stats = rows.iter().find(|r| r.get(k).cloned().flatten().as_deref() == Some("Statistics")).and_then(|r| r.get(v).cloned().flatten());
                l.row_estimate = stats.as_deref().and_then(databricks_stat_rows);
            }
        }
        Ok(l)
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let object = self
            .list_objects(schema)
            .await?
            .into_iter()
            .filter(|o| o.name == name)
            .min_by_key(|o| !o.kind.is_relation())
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let cols = self.0.schema_columns_filtered(schema, Some(name)).await?;
        let columns = cols.into_iter().next().map(|t| t.columns).unwrap_or_default();
        let full = format!("{}.{}", databrain_connector_core::quote_path(ConnectorKind::Databricks, schema), quote_ident(ConnectorKind::Databricks, name));
        let ddl = self.0.run_small(&format!("SHOW CREATE TABLE {full}")).await.ok().and_then(|r| r.first().and_then(|r| cell(r, 0)));
        Ok(ObjectDetail { object, columns, ddl, foreign_keys: vec![] })
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        self.0.schema_columns_filtered(schema, None).await
    }

    /// Two queries per catalog (tables + columns from the catalog's
    /// information schema) instead of two per schema.
    async fn bulk_metadata(&self, schemas: &[String]) -> Result<Vec<SchemaMetadata>> {
        let mut by_cat: Vec<(String, Vec<String>)> = Vec::new();
        for s in schemas {
            let (c, sch) = split_schema(s)?;
            match by_cat.iter_mut().find(|(k, _)| k == c) {
                Some((_, v)) => v.push(sch.to_string()),
                None => by_cat.push((c.to_string(), vec![sch.to_string()])),
            }
        }
        let mut out = Vec::with_capacity(schemas.len());
        for (cat, list) in by_cat {
            for chunk in list.chunks(200) {
                match self.0.catalog_metadata(&cat, chunk).await {
                    Ok(v) => out.extend(v),
                    // e.g. hive_metastore without information_schema: per schema.
                    Err(_) => {
                        let ids: Vec<String> = chunk.iter().map(|s| format!("{cat}.{s}")).collect();
                        out.extend(databrain_connector_core::default_bulk_metadata(self, &ids).await?);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Unity Catalog: per schema, relation count, latest `last_altered`
    /// (definition changes only, not data writes) and a hash of names,
    /// types and comments, in one query. Hive metastore: not supported.
    /// Tables and routines per schema (Unity Catalog; none on Hive metastore).
    async fn explorer_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        let rows = match self
            .0
            .run_small(
                "SELECT table_catalog, table_schema, concat('t', count(*), '/', cast(max(last_altered) AS STRING), '/', \
                        cast(sum(hash(table_name, table_type, comment)) AS STRING)) \
                 FROM system.information_schema.tables WHERE table_schema <> 'information_schema' GROUP BY 1, 2",
            )
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        let mut out: std::collections::HashMap<String, String> =
            rows.iter().filter_map(|r| Some((format!("{}.{}", cell(r, 0)?, cell(r, 1)?), cell(r, 2).unwrap_or_default()))).collect();
        // Functions: a new one changes the schema too (best effort: older
        // workspaces have no routines view).
        if let Ok(rows) = self
            .0
            .run_small(
                "SELECT specific_catalog, specific_schema, concat('r', count(*), '/', cast(max(last_altered) AS STRING), '/', \
                        cast(sum(hash(routine_name, routine_type)) AS STRING)) \
                 FROM system.information_schema.routines WHERE specific_schema <> 'information_schema' GROUP BY 1, 2",
            )
            .await
        {
            for r in &rows {
                if let (Some(c), Some(s)) = (cell(r, 0), cell(r, 1)) {
                    let e = out.entry(format!("{c}.{s}")).or_default();
                    e.push('|');
                    e.push_str(&cell(r, 2).unwrap_or_default());
                }
            }
        }
        Ok(Some(out))
    }

    async fn schema_object_counts(&self) -> Result<Option<std::collections::HashMap<String, usize>>> {
        let rows = match self.0.run_small("SELECT table_catalog, table_schema, count(*) FROM system.information_schema.tables GROUP BY 1, 2").await {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        Ok(Some(rows.iter().filter_map(|r| Some((format!("{}.{}", cell(r, 0)?, cell(r, 1)?), cell(r, 2)?.parse().ok()?))).collect()))
    }
}

/// `8.0 EiB`, `1.2 MiB`, `16.0 B` → bytes.
fn spark_size(s: &str) -> Option<u64> {
    let (n, unit) = s.trim().split_once(' ').unwrap_or((s.trim(), "B"));
    let n: f64 = n.parse().ok()?;
    let k: f64 = match unit.trim() {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024f64.powi(2),
        "GiB" => 1024f64.powi(3),
        "TiB" => 1024f64.powi(4),
        "PiB" => 1024f64.powi(5),
        "EiB" => 1024f64.powi(6),
        _ => return None,
    };
    Some((n * k) as u64)
}

/// `Statistics(sizeInBytes=1.2 MiB, rowCount=5.00E+4)` on a plan line.
fn line_stats(line: &str) -> Option<(Option<u64>, Option<u64>)> {
    let s = &line[line.find("Statistics(")? + "Statistics(".len()..];
    let s = &s[..s.find(')')?];
    let mut size = None;
    let mut rows = None;
    for part in s.split(", ") {
        match part.split_once('=') {
            Some(("sizeInBytes", v)) => size = spark_size(v),
            Some(("rowCount", v)) => rows = v.trim().parse::<f64>().ok().map(|r| r as u64),
            _ => {}
        }
    }
    Some((size, rows))
}

/// Spark's "don't know" size (`Long.MaxValue`, printed as 8.0 EiB).
const UNKNOWN_SIZE: u64 = 9_000_000_000_000_000_000;

/// The estimate in `EXPLAIN COST` output: bytes = the sizes of the plan's
/// leaves (the tables it reads), rows = the root's row count. A planning
/// error comes back as the plan text and is returned as the error.
fn explain_cost_estimate(text: &str) -> Result<CostEstimate> {
    if let Some(i) = text.find("Error occurred during query planning") {
        return Err(ConnectorError::query(text[i..].lines().take(6).collect::<Vec<_>>().join("\n")));
    }
    let section = text.split("== Optimized Logical Plan ==").nth(1).unwrap_or(text);
    let section = section.split("\n== ").next().unwrap_or(section);
    let lines: Vec<(usize, &str)> = section.lines().filter(|l| !l.trim().is_empty()).map(|l| (l.find(|c: char| c.is_alphanumeric()).unwrap_or(0), l)).collect();
    let mut est = CostEstimate { method: "EXPLAIN COST".into(), ..Default::default() };
    est.rows = lines.first().and_then(|(_, l)| line_stats(l)).and_then(|s| s.1);
    let mut bytes = 0u64;
    let mut unknown = false;
    let mut leaves = 0;
    for (i, (depth, l)) in lines.iter().enumerate() {
        let leaf = lines.get(i + 1).is_none_or(|(d, _)| d <= depth);
        if !leaf {
            continue;
        }
        match line_stats(l).and_then(|s| s.0) {
            Some(b) if b < UNKNOWN_SIZE => {
                bytes = bytes.saturating_add(b);
                leaves += 1;
            }
            _ => unknown = true,
        }
    }
    if leaves > 0 && !unknown {
        est.bytes = Some(bytes);
    }
    if unknown {
        est.notes.push("Some tables have no statistics, so Databricks can't size what it would read: run ANALYZE TABLE … COMPUTE STATISTICS on them.".into());
    }
    est.notes.push("From table statistics (EXPLAIN COST): a planner estimate, not the bytes a run reads after file pruning.".into());
    Ok(est)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_explain_cost() {
        let text = r"== Optimized Logical Plan ==
Aggregate [region#1], [region#1, count(1) AS n#5L], Statistics(sizeInBytes=48.0 B, rowCount=2)
+- Join Inner, (id#2 = cid#3), Statistics(sizeInBytes=3.0 MiB)
   :- Relation main.s.orders[id#2,region#1] parquet, Statistics(sizeInBytes=2.0 MiB, rowCount=5.00E+4)
   +- Relation main.s.customers[cid#3] parquet, Statistics(sizeInBytes=1.0 MiB, rowCount=1000)

== Physical Plan ==
* HashAggregate (5)
";
        let e = explain_cost_estimate(text).unwrap();
        assert_eq!((e.bytes, e.rows), (Some(3 * 1024 * 1024), Some(2)));
        let unknown =
            "== Optimized Logical Plan ==\nProject [a#1], Statistics(sizeInBytes=8.0 EiB)\n+- Relation t[a#1] json, Statistics(sizeInBytes=8.0 EiB)\n";
        let e = explain_cost_estimate(unknown).unwrap();
        assert!(e.bytes.is_none() && e.notes[0].contains("ANALYZE TABLE"), "{e:?}");
        let err = explain_cost_estimate("Error occurred during query planning: \n[TABLE_OR_VIEW_NOT_FOUND] nope").unwrap_err();
        assert!(err.message.contains("TABLE_OR_VIEW_NOT_FOUND"), "{}", err.message);
    }
}
