//! Outputs as DuckDB tables: `results.<name>` for DuckDB sessions, and
//! the types SQL sees (exact decimals kept as text become DECIMAL).

use std::sync::Arc;

use databrain_connector_core::arrow::array::{Array, ArrayRef, RecordBatch, StringArray};
use databrain_connector_core::arrow::compute::cast;
use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use databrain_connector_core::external::{ExternalTable, ExternalTables};
use databrain_result_store::{ViewSpec, display};

use super::registry::*;

/// Exposes outputs to DuckDB sessions as `results.<name>`.
pub struct OutputTables(pub Arc<OutputRegistry>);

impl ExternalTables for OutputTables {
    fn resolve(&self, name: &str) -> std::result::Result<ExternalTable, String> {
        let o = self.0.ensure_loaded(name).map_err(|e| e.message)?;
        let rs = self.0.results.get(&o.result_id).map_err(|e| e.to_string())?;
        // Larger than memory: its Parquet files (read, or streamed in).
        let disk = {
            let g = rs.lock();
            match (g.spill_files(), g.disk_sql_schema()) {
                (Some(files), Some(schema)) if g.is_complete() => Some((files, schema)),
                (Some(_), _) => return Err(format!("results.{name} is still loading; run this again when its query has finished")),
                _ => None,
            }
        };
        if let Some((files, schema)) = disk {
            let notice = o.truncated.then(|| format!("results.{name} ({}) holds only the first {} rows — totals over it may be incomplete.", o.handle, o.rows));
            return Ok(ExternalTable { name: name.to_string(), version_key: o.result_id, schema, batches: vec![], notice, files, hide: vec![] });
        }
        // Chunks with the result's own types (a single combined batch can need
        // 64-bit offsets, which DuckDB's appender would cast back and overflow).
        let (schema, chunks) = {
            let mut g = rs.lock();
            (g.schema(), g.view_batches(&ViewSpec::default(), SNAPSHOT_CHUNK).map_err(|e| e.to_string())?)
        };
        let (schema, batches) = sql_ready_all(&schema, &chunks).map_err(|e| e.to_string())?;
        let notice = o.truncated.then(|| {
            format!(
                "results.{name} ({}) holds only the first {} rows{} — totals over it may be incomplete. Re-run its query with a higher row limit (or fewer columns, when memory capped it) for complete data.",
                o.handle,
                o.rows,
                o.row_limit.map(|l| format!(" (row limit {l})")).unwrap_or_default()
            )
        });
        Ok(ExternalTable { name: name.to_string(), version_key: o.result_id, schema, batches, notice, files: vec![], hide: vec![] })
    }

    fn for_each_batch(&self, name: &str, f: &mut dyn FnMut(RecordBatch) -> std::result::Result<(), String>) -> std::result::Result<(), String> {
        let o = self.0.ensure_loaded(name).map_err(|e| e.message)?;
        let rs = self.0.results.get(&o.result_id).map_err(|e| e.to_string())?;
        let target = {
            let g = rs.lock();
            g.disk_sql_schema().unwrap_or_else(|| g.schema())
        };
        let mut failed = None;
        // Read without the result's lock (see `databrain_result_store::for_each_batch`).
        databrain_result_store::for_each_batch(&rs, &ViewSpec::default(), SNAPSHOT_CHUNK, &mut |b| {
            // The SQL types (exact decimals as DECIMAL), as resolve() said.
            let cols = b
                .columns()
                .iter()
                .zip(target.fields())
                .map(|(c, t)| if c.data_type() == t.data_type() { Ok(c.clone()) } else { cast(c, t.data_type()) })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let b = RecordBatch::try_new(target.clone(), cols)?;
            if let Err(e) = f(b) {
                failed = Some(e);
                return Err(databrain_result_store::Error::Spill("stopped".into()));
            }
            Ok(())
        })
        .map_err(|e| failed.take().unwrap_or_else(|| e.to_string()))
    }

    fn names(&self) -> Vec<String> {
        self.0.table_names()
    }

    fn catalog(&self) -> Vec<databrain_connector_core::external::ExternalInfo> {
        let mut out = Vec::new();
        for name in self.0.table_names() {
            let Some(o) = self.0.resolve(&name) else { continue };
            let what = if name == o.handle {
                "output".to_string()
            } else if o.name.as_deref() == Some(name.as_str()) {
                format!("output {}", o.handle)
            } else {
                format!("older version, {}", o.handle)
            };
            out.push(databrain_connector_core::external::ExternalInfo {
                columns: o.columns.iter().map(|c| (c.name.clone(), c.db_type.clone().unwrap_or_else(|| c.data_type.clone()))).collect(),
                rows: Some(o.rows as i64),
                comment: Some(format!(
                    "{what} · {} · {}{}",
                    o.connection_name,
                    crate::outputs::one_line(&o.sql, 120),
                    if o.truncated { " · capped (row limit or memory)" } else { "" }
                )),
                name,
            });
        }
        out
    }
}

pub(crate) fn one_line(s: &str, max: usize) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max { t } else { format!("{}…", t.chars().take(max - 1).collect::<String>()) }
}

/// Outputs as DuckDB reads them: decimals kept as text become DECIMAL
/// (precision/scale from every value of every batch) or DOUBLE.
pub fn sql_ready_all(
    schema: &SchemaRef,
    batches: &[RecordBatch],
) -> std::result::Result<(SchemaRef, Vec<RecordBatch>), databrain_connector_core::arrow::error::ArrowError> {
    let mut fields = Vec::with_capacity(schema.fields().len());
    for (i, f) in schema.fields().iter().enumerate() {
        let dt = if display::is_decimal_text(f) { decimal_type(batches.iter().map(|b| b.column(i).as_ref())) } else { f.data_type().clone() };
        fields.push(Field::new(f.name(), dt, true));
    }
    let out_schema = Arc::new(Schema::new(fields));
    let mut out = Vec::with_capacity(batches.len());
    for b in batches {
        let mut cols: Vec<ArrayRef> = Vec::with_capacity(b.num_columns());
        for (i, (f, col)) in schema.fields().iter().zip(b.columns()).enumerate() {
            let target = out_schema.field(i).data_type();
            cols.push(if display::is_decimal_text(f) {
                cast(col, target)? // the type fits every value of every batch
            } else {
                col.clone()
            });
        }
        out.push(RecordBatch::try_new(out_schema.clone(), cols)?);
    }
    Ok((out_schema, out))
}

/// [`sql_ready_all`] for one batch.
pub fn sql_ready(schema: &SchemaRef, batch: &RecordBatch) -> std::result::Result<(SchemaRef, RecordBatch), databrain_connector_core::arrow::error::ArrowError> {
    let (s, mut b) = sql_ready_all(schema, std::slice::from_ref(batch))?;
    Ok((s, b.remove(0)))
}

fn decimal_type<'a>(cols: impl Iterator<Item = &'a dyn Array>) -> DataType {
    let (mut int_digits, mut scale) = (1usize, 0usize);
    for col in cols {
        let Some(s) = col.as_any().downcast_ref::<StringArray>() else { return DataType::Float64 };
        for v in s.iter().flatten() {
            if !databrain_result_store::spill::is_plain_decimal(v) {
                return DataType::Float64;
            }
            let v = v.trim().trim_start_matches(['-', '+']);
            let (i, f) = v.split_once('.').unwrap_or((v, ""));
            int_digits = int_digits.max(i.trim_start_matches('0').len().max(1));
            scale = scale.max(f.len());
        }
    }
    let p = int_digits + scale;
    if p > 38 { DataType::Float64 } else { DataType::Decimal128(p.max(1) as u8, scale as i8) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_text_becomes_decimal() {
        let mut md = std::collections::HashMap::new();
        md.insert(databrain_connector_core::value::META_DB_TYPE.to_string(), "numeric".to_string());
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("amount", DataType::Utf8, true).with_metadata(md)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec![Some("12.50"), None, Some("-3")]))]).unwrap();
        let (s, b) = sql_ready(&schema, &batch).unwrap();
        assert_eq!(s.field(0).data_type(), &DataType::Decimal128(4, 2));
        assert_eq!(b.column(0).null_count(), 1);
    }

    /// Chunks of one output get one DECIMAL type fitting every chunk.
    #[test]
    fn decimal_type_spans_chunks() {
        let mut md = std::collections::HashMap::new();
        md.insert(databrain_connector_core::value::META_DB_TYPE.to_string(), "numeric".to_string());
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("amount", DataType::Utf8, true).with_metadata(md)]));
        let b1 = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec!["12.5"]))]).unwrap();
        let b2 = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec!["12345.125"]))]).unwrap();
        let (s, bs) = sql_ready_all(&schema, &[b1, b2]).unwrap();
        assert_eq!(s.field(0).data_type(), &DataType::Decimal128(8, 3));
        assert!(bs.iter().all(|b| b.schema() == s));
    }
}
