//! Results bigger than 32-bit offsets allow.
//!
//! A text or binary Arrow column (`Utf8`, `Binary`) addresses its values
//! with 32-bit offsets, so one array holds at most 2 GiB of them. A result
//! is received in many small batches, but filtering, sorting and paging
//! work on one combined batch: columns whose values together pass 2 GiB
//! are combined with 64-bit offsets (`LargeUtf8`, `LargeBinary`). Slices
//! handed out (pages, export chunks, outputs) are narrowed back to the
//! result's own types, which always fits for page/chunk-sized slices.

use std::sync::Arc;

use databrain_connector_core::arrow::array::{Array, AsArray, RecordBatch, UInt32Array};
use databrain_connector_core::arrow::compute::{cast, concat_batches, take};
use databrain_connector_core::arrow::datatypes::{DataType, Schema, SchemaRef};
use databrain_connector_core::arrow::error::ArrowError;

/// Bytes of values in a 32-bit-offset text/binary array (None: other types).
fn value_bytes(a: &dyn Array) -> Option<usize> {
    let o = match a.data_type() {
        DataType::Utf8 => a.as_string::<i32>().value_offsets(),
        DataType::Binary => a.as_binary::<i32>().value_offsets(),
        _ => return None,
    };
    Some((o[o.len() - 1] - o[0]) as usize)
}

fn wider(dt: &DataType) -> DataType {
    match dt {
        DataType::Utf8 => DataType::LargeUtf8,
        DataType::Binary => DataType::LargeBinary,
        d => d.clone(),
    }
}

/// Concatenate `batches`, with 64-bit offsets for columns whose values
/// pass `limit` bytes. The returned batch's schema keeps field names and
/// metadata; only those columns' types change.
pub(crate) fn concat_wide(schema: &SchemaRef, batches: &[RecordBatch], limit: usize) -> Result<RecordBatch, ArrowError> {
    let n = schema.fields().len();
    let widen: Vec<bool> = (0..n)
        .map(|c| {
            let mut total = 0usize;
            for b in batches {
                match value_bytes(b.column(c).as_ref()) {
                    Some(v) => total += v,
                    None => return false,
                }
            }
            total > limit
        })
        .collect();
    if !widen.contains(&true) {
        return concat_batches(schema, batches);
    }
    let wide = Arc::new(Schema::new_with_metadata(
        schema
            .fields()
            .iter()
            .zip(&widen)
            .map(|(f, w)| if *w { f.as_ref().clone().with_data_type(wider(f.data_type())) } else { f.as_ref().clone() })
            .collect::<Vec<_>>(),
        schema.metadata().clone(),
    ));
    let cast_batches = batches
        .iter()
        .map(|b| {
            let cols = b
                .columns()
                .iter()
                .zip(&widen)
                .map(|(col, w)| if *w { cast(col, &wider(col.data_type())) } else { Ok(col.clone()) })
                .collect::<Result<Vec<_>, _>>()?;
            RecordBatch::try_new(wide.clone(), cols)
        })
        .collect::<Result<Vec<_>, _>>()?;
    concat_batches(&wide, &cast_batches)
}

/// `batch` with the result's own column types (`schema`) again. Fails when
/// a column's values in this batch alone pass 2 GiB.
pub(crate) fn narrow(schema: &SchemaRef, batch: RecordBatch) -> Result<RecordBatch, ArrowError> {
    if batch.schema().fields().iter().zip(schema.fields()).all(|(a, b)| a.data_type() == b.data_type()) {
        return Ok(batch);
    }
    let cols = batch
        .columns()
        .iter()
        .zip(schema.fields())
        .map(|(c, f)| {
            if c.data_type() == f.data_type() {
                return Ok(c.clone());
            }
            // A slice still points into the whole column's values (offsets
            // past 2 GiB): copy just its rows first, then cast.
            let own = take(c.as_ref(), &UInt32Array::from_iter_values(0..c.len() as u32), None)?;
            cast(&own, f.data_type())
        })
        .collect::<Result<Vec<_>, _>>()?;
    RecordBatch::try_new(schema.clone(), cols)
}
