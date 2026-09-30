//! Run SQL against a stored query result locally (in-memory SQLite), so the
//! AI can compute aggregates without receiving raw rows.

use databrain_connector_core::arrow::array::{Array, RecordBatch};
use databrain_connector_core::arrow::datatypes::DataType;
use databrain_connector_core::arrow::util::display::ArrayFormatter;
use databrain_result_store::display::{format_options, is_decimal_text};
use rusqlite::types::Value as SqlValue;

use crate::types::{AiError, Result};

pub struct LocalTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub truncated: bool,
}

/// Load `batches` as table `result` and run a single read-only statement.
pub fn query(schema_names: &[String], batches: &[RecordBatch], sql: &str, max_rows: usize) -> Result<LocalTable> {
    let lower = sql.trim().to_ascii_lowercase();
    if !(lower.starts_with("select") || lower.starts_with("with")) {
        return Err(AiError::Policy("only SELECT queries are allowed on results".into()));
    }
    let conn = rusqlite::Connection::open_in_memory().map_err(|e| AiError::Internal(e.to_string()))?;
    let cols: Vec<String> = dedupe(schema_names);
    let ddl = format!(
        "CREATE TABLE result ({})",
        cols.iter().map(|c| format!("\"{}\"", c.replace('"', "\"\""))).collect::<Vec<_>>().join(", ")
    );
    conn.execute(&ddl, []).map_err(|e| AiError::Internal(e.to_string()))?;
    {
        let tx = conn.unchecked_transaction().map_err(|e| AiError::Internal(e.to_string()))?;
        let placeholders = vec!["?"; cols.len()].join(", ");
        let mut ins = tx.prepare(&format!("INSERT INTO result VALUES ({placeholders})")).map_err(|e| AiError::Internal(e.to_string()))?;
        let fopts = format_options();
        for b in batches {
            let fmts: Vec<ArrayFormatter<'_>> = b
                .columns()
                .iter()
                .map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts))
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| AiError::Internal(e.to_string()))?;
            let schema = b.schema();
            for r in 0..b.num_rows() {
                let vals: Vec<SqlValue> = b
                    .columns()
                    .iter()
                    .zip(&fmts)
                    .zip(schema.fields())
                    .map(|((a, f), field)| {
                        if a.is_null(r) {
                            return SqlValue::Null;
                        }
                        let s = f.value(r).to_string();
                        let numeric = field.data_type().is_numeric() || is_decimal_text(field);
                        match field.data_type() {
                            DataType::Boolean => SqlValue::Integer((s == "true") as i64),
                            _ if numeric => s.parse::<i64>().map(SqlValue::Integer).or_else(|_| s.parse::<f64>().map(SqlValue::Real)).unwrap_or(SqlValue::Text(s)),
                            _ => SqlValue::Text(s),
                        }
                    })
                    .collect();
                ins.execute(rusqlite::params_from_iter(vals)).map_err(|e| AiError::Internal(e.to_string()))?;
            }
        }
        drop(ins);
        tx.commit().map_err(|e| AiError::Internal(e.to_string()))?;
    }
    conn.execute_batch("PRAGMA query_only = ON").map_err(|e| AiError::Internal(e.to_string()))?;
    let mut stmt = conn.prepare(sql).map_err(|e| AiError::Policy(format!("SQL error: {e}. The table is named `result` (SQLite syntax).")))?;
    if !stmt.readonly() {
        return Err(AiError::Policy("only read-only queries are allowed on results".into()));
    }
    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let n = columns.len();
    let mut rows_iter = stmt.query([]).map_err(|e| AiError::Policy(e.to_string()))?;
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(r) = rows_iter.next().map_err(|e| AiError::Policy(e.to_string()))? {
        if rows.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut row = Vec::with_capacity(n);
        for i in 0..n {
            row.push(match r.get_ref(i).map_err(|e| AiError::Internal(e.to_string()))? {
                rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                rusqlite::types::ValueRef::Integer(i) => i.into(),
                rusqlite::types::ValueRef::Real(f) => serde_json::Number::from_f64(f).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null),
                rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
                rusqlite::types::ValueRef::Blob(_) => "<binary>".into(),
            });
        }
        rows.push(row);
    }
    Ok(LocalTable { columns, rows, truncated })
}

fn dedupe(names: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    names
        .iter()
        .map(|n| {
            let k = n.to_ascii_lowercase();
            let c = seen.entry(k).or_insert(0);
            *c += 1;
            if *c == 1 { n.clone() } else { format!("{n}_{c}") }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::arrow::array::{Float64Array, Int64Array, StringArray};
    use databrain_connector_core::arrow::datatypes::{Field, Schema};
    use std::sync::Arc;

    #[test]
    fn aggregates_locally() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("region", DataType::Utf8, true),
            Field::new("amount", DataType::Float64, true),
            Field::new("id", DataType::Int64, true),
        ]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["EU", "US", "EU"])),
                Arc::new(Float64Array::from(vec![10.0, 5.0, 2.5])),
                Arc::new(Int64Array::from(vec![1, 2, 3])),
            ],
        )
        .unwrap();
        let names: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
        let t = query(&names, &[b.clone()], "select region, sum(amount) s from result group by 1 order by 1", 10).unwrap();
        assert_eq!(t.columns, vec!["region", "s"]);
        assert_eq!(t.rows[0], vec![serde_json::json!("EU"), serde_json::json!(12.5)]);
        assert!(query(&names, &[b.clone()], "delete from result", 10).is_err());
        assert!(query(&names, &[b], "select * from result", 1).unwrap().truncated);
    }
}
