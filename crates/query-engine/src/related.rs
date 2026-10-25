//! Rows related to a result's rows through foreign keys: the row a value
//! references (parent) and the rows of other tables that reference a row
//! (children). Works for results of a query on one table (see
//! [`crate::edit::source_table`]).
//!
//! - Parents come from the table's own foreign keys (`describe`).
//! - Children come from the AI knowledge index when the connection has one
//!   (every indexed schema); otherwise the table's own schema is read.

use databrain_connector_core::arrow::datatypes::SchemaRef;
use databrain_connector_core::{quote_ident, quote_path};
use databrain_export::load::edit_literal;
use serde::{Deserialize, Serialize};

use crate::{EngineError, OutputInfo, QueryEngine, Result};

/// Most rows a related-rows query asks for at once.
pub const MAX_RELATED_ROWS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkDirection {
    /// The result's rows reference `table` (open the referenced row).
    Parent,
    /// Rows of `table` reference the result's rows.
    Children,
}

/// A foreign key between the result's table and another table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultLink {
    pub direction: LinkDirection,
    /// The other table (schema id as the explorer names it).
    pub schema: String,
    pub table: String,
    /// Result column indexes holding the values to look up.
    pub columns: Vec<usize>,
    /// Columns of `table` compared with them, in the same order.
    pub target_columns: Vec<String>,
}

impl QueryEngine {
    /// Foreign keys from and to the table a result was read from. Empty when
    /// the result is not from one table or its key columns are not in it.
    pub async fn result_links(&self, o: &OutputInfo, schema: &SchemaRef) -> Result<Vec<ResultLink>> {
        let Ok(src) = self.resolve_source(o, schema).await else { return Ok(vec![]) };
        let position = |col: &str| src.columns.iter().position(|c| c.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(col)));
        let mut out = Vec::new();
        for fk in &src.detail.foreign_keys {
            let Some(columns) = fk.columns.iter().map(|c| position(c)).collect::<Option<Vec<_>>>() else { continue };
            let schema = if fk.ref_schema.is_empty() { src.schema.clone() } else { fk.ref_schema.clone() };
            out.push(ResultLink { direction: LinkDirection::Parent, schema, table: fk.ref_table.clone(), columns, target_columns: fk.ref_columns.clone() });
        }
        let table = &src.detail.object.name;
        let incoming = match self.workspace().kn_foreign_keys_to(&o.connection_id, table)? {
            Some(v) => v,
            None => self
                .bulk_metadata(&o.connection_id, std::slice::from_ref(&src.schema))
                .await
                .unwrap_or_default()
                .into_iter()
                .flat_map(|m| {
                    let s = m.schema;
                    m.columns.into_iter().flat_map(move |t| {
                        let s = s.clone();
                        t.foreign_keys.into_iter().map(move |fk| (s.clone(), t.table.clone(), fk))
                    })
                })
                .filter(|(_, _, fk)| fk.ref_table.eq_ignore_ascii_case(table))
                .collect(),
        };
        for (schema, child, fk) in incoming {
            let ref_schema = if fk.ref_schema.is_empty() { schema.as_str() } else { fk.ref_schema.as_str() };
            if !ref_schema.eq_ignore_ascii_case(&src.schema) {
                continue;
            }
            let Some(columns) = fk.ref_columns.iter().map(|c| position(c)).collect::<Option<Vec<_>>>() else { continue };
            out.push(ResultLink { direction: LinkDirection::Children, schema, table: child, columns, target_columns: fk.columns });
        }
        Ok(out)
    }

    /// `SELECT * FROM` the link's table for the given rows' values (one value
    /// list per row, in `link.columns` order). Rows with a NULL are skipped.
    pub fn related_rows_sql(&self, connection_id: &str, schema: &SchemaRef, link: &ResultLink, rows: &[Vec<Option<String>>]) -> Result<String> {
        let kind = self.workspace().get_connection(connection_id)?.config.kind;
        let bad = |m: String| EngineError::new("invalid", m);
        if link.columns.is_empty() || link.columns.len() != link.target_columns.len() {
            return Err(bad("The link has no columns".into()));
        }
        let fields = link.columns.iter().map(|&i| schema.fields().get(i).cloned().ok_or_else(|| bad(format!("Column {i} is not in the result")))).collect::<Result<Vec<_>>>()?;
        let cols: Vec<String> = link.target_columns.iter().map(|c| quote_ident(kind, c)).collect();
        let mut tuples: Vec<Vec<String>> = Vec::new();
        for r in rows.iter().take(MAX_RELATED_ROWS) {
            if r.len() != fields.len() || r.iter().any(Option::is_none) {
                continue;
            }
            let lits = fields.iter().zip(r).map(|(f, v)| edit_literal(f, v.as_deref(), kind).map_err(bad)).collect::<Result<Vec<_>>>()?;
            if !tuples.contains(&lits) {
                tuples.push(lits);
            }
        }
        if tuples.is_empty() {
            return Err(bad("The selected rows have no values to look up (NULL)".into()));
        }
        let cond = if cols.len() == 1 {
            let vals: Vec<&str> = tuples.iter().map(|t| t[0].as_str()).collect();
            if vals.len() == 1 { format!("{} = {}", cols[0], vals[0]) } else { format!("{} IN ({})", cols[0], vals.join(", ")) }
        } else {
            tuples
                .iter()
                .map(|t| format!("({})", cols.iter().zip(t).map(|(c, v)| format!("{c} = {v}")).collect::<Vec<_>>().join(" AND ")))
                .collect::<Vec<_>>()
                .join("\n   OR ")
        };
        let table = quote_path(kind, &format!("{}.{}", link.schema, link.table));
        Ok(format!("SELECT *\nFROM {table}\nWHERE {cond}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    #[test]
    fn link_sql_quotes_and_dedups_values() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Arc::new(databrain_workspace::Workspace::open(dir.path().join("w.db")).unwrap());
        let mut cfg = databrain_connector_core::ConnectionConfig::new(databrain_connector_core::ConnectorKind::Postgres, databrain_auth::AuthMethod::None);
        cfg.host = Some("h".into());
        let p = ws
            .save_connection(databrain_workspace::ConnectionProfile { id: String::new(), name: "pg".into(), config: cfg, color: None, env: Default::default(), folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 })
            .unwrap();
        let engine = QueryEngine::new(
            databrain_connector_core::ConnectorRegistry::new(),
            ws,
            Arc::new(databrain_auth::MemoryStore::default()),
            Arc::new(databrain_result_store::ResultStore::new()),
            Arc::new(crate::CollectingSink::default()),
        );
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("customer_id", DataType::Int64, true), Field::new("code", DataType::Utf8, true)]));
        let one = ResultLink { direction: LinkDirection::Parent, schema: "public".into(), table: "customers".into(), columns: vec![0], target_columns: vec!["id".into()] };
        let rows = vec![vec![Some("7".into())], vec![Some("7".into())], vec![None], vec![Some("9".into())]];
        assert_eq!(engine.related_rows_sql(&p.id, &schema, &one, &rows).unwrap(), "SELECT *\nFROM \"public\".\"customers\"\nWHERE \"id\" IN (7, 9)");
        assert!(engine.related_rows_sql(&p.id, &schema, &one, &[vec![Some("x; drop".into())]]).is_err(), "numbers are checked");
        let two = ResultLink { columns: vec![0, 1], target_columns: vec!["id".into(), "Code".into()], ..one };
        assert_eq!(
            engine.related_rows_sql(&p.id, &schema, &two, &[vec![Some("1".into()), Some("o'k".into())]]).unwrap(),
            "SELECT *\nFROM \"public\".\"customers\"\nWHERE (\"id\" = 1 AND \"Code\" = 'o''k')"
        );
    }
}
