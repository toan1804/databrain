//! Catalog/schema metadata queries (columns, fingerprints, per-catalog bulk loads).

use super::client::Inner;
use databrain_connector_core::{
    ColumnInfo, ConnectorError, ConnectorKind, DbObject, ObjectKind, Result, SchemaMetadata, TableColumns, quote_ident, quote_literal,
};

pub(super) fn split_schema(path: &str) -> Result<(&str, &str)> {
    path.split_once('.').ok_or_else(|| ConnectorError::query(format!("expected catalog.schema, got {path}")))
}

pub(super) fn cell(r: &[serde_json::Value], i: usize) -> Option<String> {
    r.get(i).and_then(|v| v.as_str()).map(str::to_string)
}

impl Inner {
    pub(super) async fn catalog_metadata(&self, cat: &str, schemas: &[String]) -> Result<Vec<SchemaMetadata>> {
        let list = schemas.iter().map(|s| quote_literal(s)).collect::<Vec<_>>().join(", ");
        let cq = quote_ident(ConnectorKind::Databricks, cat);
        let tables = self
            .run_small(&format!("SELECT table_schema, table_name, table_type, comment FROM {cq}.information_schema.tables WHERE table_schema IN ({list})"))
            .await?;
        let cols = self
            .run_small(&format!(
                "SELECT table_schema, table_name, column_name, full_data_type, is_nullable, comment FROM {cq}.information_schema.columns \
                 WHERE table_schema IN ({list}) ORDER BY table_schema, table_name, ordinal_position"
            ))
            .await?;
        let mut out: Vec<SchemaMetadata> = schemas
            .iter()
            .map(|s| SchemaMetadata { schema: format!("{cat}.{s}"), objects: vec![], columns: vec![], error: None, foreign_keys_read: false })
            .collect();
        let idx = |s: &str| schemas.iter().position(|x| x == s);
        for r in &tables {
            let (Some(s), Some(name)) = (cell(r, 0), cell(r, 1)) else { continue };
            let Some(i) = idx(&s) else { continue };
            let t = cell(r, 2).unwrap_or_default();
            let schema = out[i].schema.clone();
            out[i].objects.push(DbObject {
                schema,
                name,
                kind: if t.contains("VIEW") {
                    if t.contains("MATERIALIZED") { ObjectKind::MaterializedView } else { ObjectKind::View }
                } else {
                    ObjectKind::Table
                },
                comment: cell(r, 3),
                row_estimate: None,
            });
        }
        for r in &cols {
            let (Some(s), Some(table)) = (cell(r, 0), cell(r, 1)) else { continue };
            let Some(i) = idx(&s) else { continue };
            let col = ColumnInfo {
                name: cell(r, 2).unwrap_or_default(),
                data_type: cell(r, 3).unwrap_or_default(),
                nullable: cell(r, 4).as_deref() != Some("NO"),
                is_primary_key: false,
                default: None,
                comment: cell(r, 5),
            };
            let cs = &mut out[i].columns;
            match cs.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => cs.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        Ok(out)
    }

    pub(super) async fn schema_columns_filtered(&self, schema: &str, table: Option<&str>) -> Result<Vec<TableColumns>> {
        let (cat, sch) = split_schema(schema)?;
        let mut sql = format!(
            "SELECT table_name, column_name, full_data_type, is_nullable, comment FROM {}.information_schema.columns WHERE table_schema = {}",
            quote_ident(ConnectorKind::Databricks, cat),
            quote_literal(sch)
        );
        if let Some(t) = table {
            sql.push_str(&format!(" AND table_name = {}", quote_literal(t)));
        }
        sql.push_str(" ORDER BY table_name, ordinal_position");
        let rows = self.run_small(&sql).await?;
        let mut out: Vec<TableColumns> = Vec::new();
        for r in rows {
            let table = cell(&r, 0).unwrap_or_default();
            let col = ColumnInfo {
                name: cell(&r, 1).unwrap_or_default(),
                data_type: cell(&r, 2).unwrap_or_default(),
                nullable: cell(&r, 3).as_deref() != Some("NO"),
                is_primary_key: false,
                default: None,
                comment: cell(&r, 4),
            };
            match out.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => out.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        Ok(out)
    }
}

/// Rows from a `Statistics` line of DESCRIBE TABLE EXTENDED (`1234 bytes, 56 rows`).
pub(super) fn databricks_stat_rows(stats: &str) -> Option<i64> {
    stats.split(',').map(str::trim).find_map(|part| part.strip_suffix(" rows")).and_then(|n| n.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rows_from_statistics() {
        assert_eq!(databricks_stat_rows("123456 bytes, 789 rows"), Some(789));
        assert_eq!(databricks_stat_rows("123456 bytes"), None);
    }
}
