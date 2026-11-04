//! `Session` implementation: execution and catalog browsing.

use async_trait::async_trait;

use serde_json::json;

use databrain_connector_core::{
    ConnectorError, ConnectorKind, DbObject, ExecOptions, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, SchemaMetadata, Session, TableColumns,
    quote_ident, quote_literal,
};

use super::client::DbxSession;
use super::metadata::{cell, databricks_stat_rows, split_schema};

#[async_trait]
impl Session for DbxSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Databricks
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.0.run_small("SELECT current_version().dbsql_version").await.unwrap_or_default();
        let v = r.first().and_then(|r| cell(r, 0)).unwrap_or_default();
        Ok(format!("Databricks SQL {v} (warehouse {})", self.0.warehouse).replace("  ", " "))
    }

    async fn ping(&self) -> Result<()> {
        self.0.run_small("SELECT 1").await.map(|_| ())
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
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

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        use databrain_connector_core::{TableLayout, parse_list_text, query_table};
        let full = format!("{}.{}", databrain_connector_core::quote_path(ConnectorKind::Databricks, schema), quote_ident(ConnectorKind::Databricks, name));
        let mut l = TableLayout::default();
        // Views and non-Delta tables have no detail: no hints, not an error.
        let Ok((names, rows)) = query_table(self, &format!("DESCRIBE DETAIL {full}")).await else { return Ok(l) };
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
        if let Ok((names, rows)) = query_table(self, &format!("DESCRIBE TABLE EXTENDED {full}")).await {
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
    async fn schema_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        let rows = match self
            .0
            .run_small(
                "SELECT table_catalog, table_schema, concat(count(*), '/', cast(max(last_altered) AS STRING), '/', \
                        cast(sum(hash(table_name, table_type, comment)) AS STRING)) \
                 FROM system.information_schema.tables WHERE table_schema <> 'information_schema' GROUP BY 1, 2",
            )
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        Ok(Some(rows.iter().filter_map(|r| Some((format!("{}.{}", cell(r, 0)?, cell(r, 1)?), cell(r, 2).unwrap_or_default()))).collect()))
    }

    async fn schema_object_counts(&self) -> Result<Option<std::collections::HashMap<String, usize>>> {
        let rows = match self.0.run_small("SELECT table_catalog, table_schema, count(*) FROM system.information_schema.tables GROUP BY 1, 2").await {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        Ok(Some(rows.iter().filter_map(|r| Some((format!("{}.{}", cell(r, 0)?, cell(r, 1)?), cell(r, 2)?.parse().ok()?))).collect()))
    }
}
