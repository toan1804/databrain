//! Rows of documents or records (MongoDB documents, Neo4j records) to Arrow.
//!
//! Each row is a list of `(field, Cell)`. Columns are the fields in the order
//! they are first seen; a column's Arrow type is inferred from the first
//! `infer_rows` rows (all ints → Int64, ints and floats → Float64, one other
//! type → that type, anything mixed → text). Nested values (objects, arrays,
//! nodes) are JSON text with their kind as the column's database type, so
//! the grid, outputs (DuckDB `->>`), export and the AI work on them as is.
//! Fields that only appear after the inferred rows are dropped with a notice.

use std::collections::{BTreeSet, HashMap};

use crate::error::Result;
use crate::stream::StreamEvent;
use crate::value::{BatchBuilder, ColType, Column, Value};

/// One value with the name its database gives its type (`objectId`, `date`,
/// `object`, `array`, `node`, `int`…).
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub value: Value,
    pub db_type: &'static str,
}

impl Cell {
    pub fn new(value: Value, db_type: &'static str) -> Self {
        Self { value, db_type }
    }
    pub fn null() -> Self {
        Self { value: Value::Null, db_type: "null" }
    }
    /// Nested value as JSON text.
    pub fn json(v: &serde_json::Value, db_type: &'static str) -> Self {
        Self { value: Value::Text(v.to_string()), db_type }
    }
}

pub type Row = Vec<(String, Cell)>;

/// Rows per inferred schema.
pub const INFER_ROWS: usize = 1000;

pub struct RowBatcher {
    batch_size: usize,
    infer_rows: usize,
    pending: Vec<Row>,
    built: Option<Built>,
    dropped: BTreeSet<String>,
    rows: u64,
}

struct Built {
    index: HashMap<String, usize>,
    width: usize,
    builder: BatchBuilder,
}

impl RowBatcher {
    pub fn new(batch_size: usize) -> Self {
        Self::with_inference(batch_size, INFER_ROWS)
    }

    pub fn with_inference(batch_size: usize, infer_rows: usize) -> Self {
        Self { batch_size: batch_size.max(1), infer_rows: infer_rows.max(1), pending: Vec::new(), built: None, dropped: BTreeSet::new(), rows: 0 }
    }

    /// Rows pushed so far.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Add a row; returns the events to send now (schema, full batches).
    pub fn push(&mut self, row: Row) -> Result<Vec<StreamEvent>> {
        self.rows += 1;
        let mut out = Vec::new();
        if self.built.is_none() {
            self.pending.push(row);
            if self.pending.len() >= self.infer_rows {
                out.extend(self.build()?);
            }
            return Ok(out);
        }
        self.append(row);
        if self.built.as_ref().is_some_and(|b| b.builder.is_full()) {
            out.push(StreamEvent::Batch(self.built.as_mut().expect("built").builder.finish()?));
        }
        Ok(out)
    }

    /// The remaining events (schema when not sent yet, last batch, notices).
    pub fn finish(mut self) -> Result<Vec<StreamEvent>> {
        let mut out = Vec::new();
        if self.built.is_none() {
            if self.pending.is_empty() {
                return Ok(out);
            }
            out.extend(self.build()?);
        }
        let b = self.built.as_mut().expect("built");
        if !b.builder.is_empty() {
            out.push(StreamEvent::Batch(b.builder.finish()?));
        }
        let failures = b.builder.coercion_failures();
        if failures > 0 {
            out.push(StreamEvent::Notice(format!("{failures} values didn't match their column's type (inferred from the first {} rows) and are shown as NULL", self.infer_rows)));
        }
        if !self.dropped.is_empty() {
            let names: Vec<&str> = self.dropped.iter().take(20).map(String::as_str).collect();
            out.push(StreamEvent::Notice(format!(
                "Fields that first appear after the first {} rows are not shown: {}{}",
                self.infer_rows,
                names.join(", "),
                if self.dropped.len() > 20 { ", …" } else { "" }
            )));
        }
        Ok(out)
    }

    fn build(&mut self) -> Result<Vec<StreamEvent>> {
        let mut names: Vec<String> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut seen: Vec<Vec<(ColType, &'static str)>> = Vec::new();
        for row in &self.pending {
            for (k, c) in row {
                let i = *index.entry(k.clone()).or_insert_with(|| {
                    names.push(k.clone());
                    seen.push(Vec::new());
                    names.len() - 1
                });
                if let Some(t) = col_type(&c.value) {
                    if !seen[i].contains(&(t, c.db_type)) {
                        seen[i].push((t, c.db_type));
                    }
                }
            }
        }
        let columns: Vec<Column> = names.iter().zip(&seen).map(|(n, s)| infer(n, s)).collect();
        let builder = BatchBuilder::new(&columns, self.batch_size);
        let schema = builder.schema();
        self.built = Some(Built { width: columns.len(), index, builder });
        let mut out = vec![StreamEvent::Schema(schema)];
        for row in std::mem::take(&mut self.pending) {
            self.append(row);
            let b = self.built.as_mut().expect("built");
            if b.builder.is_full() {
                out.push(StreamEvent::Batch(b.builder.finish()?));
            }
        }
        Ok(out)
    }

    fn append(&mut self, row: Row) {
        let b = self.built.as_mut().expect("built");
        let mut values = vec![Value::Null; b.width];
        for (k, c) in row {
            match b.index.get(&k) {
                Some(&i) => values[i] = c.value,
                None => {
                    if !matches!(c.value, Value::Null) {
                        self.dropped.insert(k);
                    }
                }
            }
        }
        b.builder.push_row(values);
    }
}

fn col_type(v: &Value) -> Option<ColType> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(_) => ColType::Bool,
        Value::Int(_) => ColType::Int64,
        Value::UInt(_) => ColType::UInt64,
        Value::Float(_) => ColType::Float64,
        Value::Text(_) => ColType::Utf8,
        Value::Bytes(_) => ColType::Binary,
        Value::Date(_) => ColType::Date,
        Value::Timestamp(_) => ColType::Timestamp,
        Value::TimestampTz(_) => ColType::TimestampTz,
        Value::Time(_) => ColType::Time,
    })
}

fn infer(name: &str, seen: &[(ColType, &'static str)]) -> Column {
    let mut types: Vec<ColType> = seen.iter().map(|s| s.0).collect();
    types.dedup();
    let mut db: Vec<&str> = seen.iter().map(|s| s.1).collect();
    db.sort();
    db.dedup();
    let db_type = match db.as_slice() {
        [] => "null".to_string(),
        [one] => one.to_string(),
        many => format!("mixed ({})", many.join(", ")),
    };
    let all = |t: &[ColType]| seen.iter().all(|s| t.contains(&s.0));
    let t = if seen.is_empty() {
        ColType::Utf8
    } else if seen.iter().all(|s| s.0 == seen[0].0) {
        seen[0].0
    } else if all(&[ColType::Int64, ColType::Float64, ColType::UInt64]) {
        ColType::Float64
    } else if all(&[ColType::Int64, ColType::UInt64]) {
        ColType::Int64
    } else if all(&[ColType::Timestamp, ColType::TimestampTz, ColType::Date]) {
        ColType::TimestampTz
    } else {
        ColType::Utf8
    };
    Column::new(name, t, db_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Array;

    fn row(cells: &[(&str, Cell)]) -> Row {
        cells.iter().map(|(k, c)| (k.to_string(), c.clone())).collect()
    }

    #[test]
    fn infers_columns_from_documents() {
        let mut b = RowBatcher::with_inference(2, 3);
        let int = |i| Cell::new(Value::Int(i), "int");
        let mut ev = vec![];
        ev.extend(b.push(row(&[("_id", Cell::new(Value::Text("a1".into()), "objectId")), ("n", int(1)), ("tags", Cell::json(&serde_json::json!(["x"]), "array"))])).unwrap());
        ev.extend(b.push(row(&[("_id", Cell::new(Value::Text("a2".into()), "objectId")), ("n", Cell::new(Value::Float(2.5), "double"))])).unwrap());
        ev.extend(b.push(row(&[("_id", Cell::new(Value::Text("a3".into()), "objectId")), ("n", Cell::new(Value::Text("x".into()), "string")), ("late", int(1))])).unwrap());
        // Built after 3 rows: schema + one full batch of 2.
        let StreamEvent::Schema(s) = &ev[0] else { panic!("{ev:?}") };
        let names: Vec<&str> = s.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["_id", "n", "tags", "late"]);
        assert_eq!(s.field(1).data_type(), &arrow::datatypes::DataType::Utf8, "int, double and string mixed → text");
        assert_eq!(s.field(1).metadata().get(crate::value::META_DB_TYPE).unwrap(), "mixed (double, int, string)");
        assert_eq!(s.field(2).metadata().get(crate::value::META_DB_TYPE).unwrap(), "array");
        assert!(matches!(&ev[1], StreamEvent::Batch(r) if r.num_rows() == 2));
        // The 4th row fills the second batch (rows 3 and 4).
        let more = b.push(row(&[("_id", Cell::new(Value::Text("a4".into()), "objectId")), ("new_field", int(5))])).unwrap();
        let StreamEvent::Batch(last) = &more[0] else { panic!("{more:?}") };
        assert_eq!(last.num_rows(), 2);
        assert!(last.column(1).is_valid(0), "the string 'x' kept in the text column");
        let rest = b.finish().unwrap();
        assert!(matches!(&rest[..], [StreamEvent::Notice(n)] if n.contains("new_field")), "{rest:?}");
    }

    #[test]
    fn numbers_and_empty() {
        let mut b = RowBatcher::new(100);
        b.push(row(&[("x", Cell::new(Value::Int(1), "int"))])).unwrap();
        b.push(row(&[("x", Cell::new(Value::Float(1.5), "double"))])).unwrap();
        let ev = b.finish().unwrap();
        let StreamEvent::Schema(s) = &ev[0] else { panic!() };
        assert_eq!(s.field(0).data_type(), &arrow::datatypes::DataType::Float64);
        assert!(RowBatcher::new(10).finish().unwrap().is_empty());
    }
}
