//! Views (filters, quick filter, sort), find and column statistics of a
//! result on disk, file by file: the rows are never all in memory. Filters
//! and text matching are the in-memory code run on each file (same answers);
//! sorting goes to the [`DiskSorter`] (DuckDB, out of memory) with the keys
//! the in-memory sort compares, and the sorted rows are kept on disk too.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use databrain_connector_core::arrow::array::{Array, ArrayRef, AsArray, BinaryArray, RecordBatch, UInt32Array, UInt64Array};
use databrain_connector_core::arrow::compute;
use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef, UInt64Type};

use crate::display::{self, is_decimal_text};
use crate::spill::{CHUNK_ROWS, Spill};
use crate::view::{self, DisplayCache, ViewSpec};
use crate::{CellMatch, ColumnStats, DiskView, Error, FindResult, Result, ResultSet, SortJob, TopValue, decimal};

/// Distinct values counted for column statistics of a result on disk.
const MAX_DISTINCT: usize = 1_000_000;

static VIEW_SEQ: AtomicU64 = AtomicU64::new(0);

/// `schema` plus the `__row_id` column (the row's place in the result).
fn with_row_id(schema: &SchemaRef) -> SchemaRef {
    let mut fields: Vec<Field> = schema.fields().iter().map(|f| f.as_ref().clone()).collect();
    fields.push(Field::new("__row_id", DataType::UInt64, false));
    Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

/// Split the `__row_id` column off a view batch.
pub(crate) fn split_ids(b: &RecordBatch, schema: &SchemaRef) -> Result<(RecordBatch, Vec<u32>)> {
    let n = b.num_columns() - 1;
    let ids = b.column(n).as_primitive::<UInt64Type>().values().iter().map(|&i| i as u32).collect();
    Ok((RecordBatch::try_new(schema.clone(), b.columns()[..n].to_vec())?, ids))
}

/// Stops a `for_each` early (not an error).
fn stop() -> Error {
    Error::Spill("\u{0}stop".into())
}

fn stopped(r: Result<()>) -> Result<()> {
    match r {
        Err(Error::Spill(m)) if m == "\u{0}stop" => Ok(()),
        r => r,
    }
}

impl ResultSet {
    /// The view of a result on disk for `spec` (not the identity), built once
    /// per spec: a [`Spill`] of the kept rows in order, plus `__row_id`.
    pub(crate) fn disk_view(&mut self, spec: &ViewSpec) -> Result<&mut Spill> {
        if self.disk_view.as_ref().is_some_and(|v| &v.spec == spec) {
            return Ok(&mut self.disk_view.as_mut().expect("checked").rows);
        }
        if !self.complete {
            return Err(Error::OnDisk("Sorting and filtering while rows are still loading"));
        }
        // The previous view's files go first.
        self.disk_view = None;
        let sorter = self.sorter.clone();
        let schema = self.schema.clone();
        let Some(data) = self.spill.as_mut() else { return Err(Error::OnDisk("This")) };
        let ext = with_row_id(&schema);
        let dir = data.dir().join(format!("view-{}", VIEW_SEQ.fetch_add(1, Ordering::Relaxed)));
        let mut base = 0u64;
        let rows = if spec.sort.is_empty() {
            let mut out = Spill::create(dir, ext.clone())?;
            data.for_each(CHUNK_ROWS, &mut |b| {
                let n = b.num_rows() as u64;
                let kept = view::kept_rows(&b, spec, &mut DisplayCache::default())?;
                let (rows, ids) = match kept {
                    None => (b, UInt64Array::from_iter_values(base..base + n)),
                    Some(idx) => (compute::take_record_batch(&b, &idx)?, UInt64Array::from_iter_values(idx.values().iter().map(|&i| base + i as u64))),
                };
                base += n;
                let mut cols = rows.columns().to_vec();
                cols.push(Arc::new(ids));
                out.push(RecordBatch::try_new(ext.clone(), cols)?)
            })?;
            out.finish()?;
            out
        } else {
            let sorter = sorter.ok_or(Error::OnDisk("Sorting (it needs DuckDB)"))?;
            std::fs::create_dir_all(&dir).map_err(|e| Error::Spill(e.to_string()))?;
            let mut key_fields = vec![Field::new("__row_id", DataType::UInt64, false)];
            for (i, k) in spec.sort.iter().enumerate() {
                let f = schema.fields().get(k.column).ok_or(Error::InvalidColumn(k.column))?;
                let dt = if is_decimal_text(f) { DataType::Binary } else { f.data_type().clone() };
                key_fields.push(Field::new(format!("k{i}"), dt, true));
            }
            let key_schema: SchemaRef = Arc::new(Schema::new(key_fields));
            let mut keys = Spill::create(dir.join("keys"), key_schema.clone())?;
            data.for_each(CHUNK_ROWS, &mut |b| {
                let n = b.num_rows() as u64;
                let idx = view::kept_rows(&b, spec, &mut DisplayCache::default())?.unwrap_or_else(|| UInt32Array::from_iter_values(0..n as u32));
                let mut cols: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from_iter_values(idx.values().iter().map(|&i| base + i as u64)))];
                for k in view::sort_keys(&b, spec)? {
                    cols.push(compute::take(k.as_ref(), &idx, None)?);
                }
                base += n;
                keys.push(RecordBatch::try_new(key_schema.clone(), cols)?)
            })?;
            keys.finish()?;
            let out = dir.join("sorted.parquet");
            let job = SortJob { keys: keys.files(), data: data.file_starts(), descending: spec.sort.iter().map(|k| k.descending).collect(), out: out.clone() };
            sorter.sort(&job).map_err(Error::Spill)?;
            drop(keys);
            Spill::from_file(dir, out, ext)?
        };
        self.disk_view = Some(DiskView { spec: spec.clone(), rows });
        Ok(&mut self.disk_view.as_mut().expect("just set").rows)
    }

    /// The rows of `spec` (the result itself for the identity view).
    fn disk_rows(&mut self, spec: &ViewSpec) -> Result<&mut Spill> {
        if spec.is_identity() {
            return self.spill.as_mut().ok_or(Error::OnDisk("This"));
        }
        self.disk_view(spec)
    }

    /// Every view batch of a result on disk, without `__row_id`.
    pub(crate) fn disk_for_each(&mut self, spec: &ViewSpec, chunk: usize, f: &mut dyn FnMut(RecordBatch) -> Result<()>) -> Result<()> {
        let schema = self.schema.clone();
        let identity = spec.is_identity();
        self.disk_rows(spec)?.for_each(chunk, &mut |b| if identity { f(b) } else { f(split_ids(&b, &schema)?.0) })
    }

    /// [`ResultSet::find_with`] for a result on disk, a file at a time.
    pub(crate) fn disk_find(&mut self, spec: &ViewSpec, query: &str, columns: Option<&[usize]>, limit: usize, whole_word: bool) -> Result<FindResult> {
        let ncols = self.schema.fields().len();
        let cols: Vec<usize> = match columns {
            Some(c) if !c.is_empty() => (0..ncols).filter(|i| c.contains(i)).collect(),
            _ => (0..ncols).collect(),
        };
        let pattern = format!("%{}%", view::like_escape(query));
        let needle = query.to_lowercase();
        let (mut matches, mut truncated, mut at) = (Vec::new(), false, 0usize);
        let r = self.disk_rows(spec)?.for_each(CHUNK_ROWS, &mut |b| {
            let mut cache = DisplayCache::default();
            let mut masks: Vec<Option<(Arc<display::DisplayText>, _)>> = vec![None; ncols];
            for &c in &cols {
                let text = cache.get(&b, c)?;
                let m = text.ilike(&pattern, false)?;
                masks[c] = Some((text, m));
            }
            for r in 0..b.num_rows() {
                for (c, m) in masks.iter().enumerate() {
                    let Some((text, m)) = m else { continue };
                    let hit = m.is_valid(r) && m.value(r) && (!whole_word || crate::contains_word(&text.value(r).to_lowercase(), &needle));
                    if hit {
                        if matches.len() >= limit {
                            truncated = true;
                            return Err(stop());
                        }
                        matches.push(CellMatch { row: (at + r) as u32, col: c as u32 });
                    }
                }
            }
            at += b.num_rows();
            Ok(())
        });
        stopped(r)?;
        Ok(FindResult { matches, truncated })
    }

    /// [`ResultSet::column_stats`] for a result on disk, a file at a time
    /// (distinct values counted up to a million).
    pub(crate) fn disk_column_stats(&mut self, spec: &ViewSpec, column: usize) -> Result<ColumnStats> {
        let field = self.schema.field(column).clone();
        let decimal_text = is_decimal_text(&field);
        let (mut count, mut nulls) = (0u64, 0u64);
        // Each file's smallest and largest value (as compared, and as shown).
        let mut ends: Vec<(ArrayRef, String)> = Vec::new();
        let mut counts: HashMap<Option<Vec<u8>>, (Option<String>, u64)> = HashMap::new();
        self.disk_rows(spec)?.for_each(CHUNK_ROWS, &mut |b| {
            let col = b.column(column).clone();
            count += col.len() as u64;
            nulls += col.null_count() as u64;
            let sortable = if decimal_text { Arc::new(decimal::keys(col.as_ref())?) as ArrayRef } else { col.clone() };
            let text = display::to_display(col.as_ref())?;
            if col.len() > col.null_count()
                && let Ok(order) = compute::sort_to_indices(sortable.as_ref(), Some(compute::SortOptions { descending: false, nulls_first: false }), None)
            {
                let non_null: Vec<u32> = order.values().iter().copied().filter(|i| !sortable.is_null(*i as usize)).collect();
                for i in [non_null.first(), non_null.last()].into_iter().flatten() {
                    ends.push((sortable.slice(*i as usize, 1), text.value(*i as usize).to_string()));
                }
            }
            let keys = decimal_text.then(|| sortable.as_any().downcast_ref::<BinaryArray>().cloned()).flatten();
            for i in 0..text.len() {
                let k = if text.is_null(i) {
                    None
                } else {
                    Some(match &keys {
                        Some(k) if k.is_valid(i) => k.value(i).to_vec(),
                        _ => text.value(i).as_bytes().to_vec(),
                    })
                };
                if let Some(e) = counts.get_mut(&k) {
                    e.1 += 1;
                } else if counts.len() < MAX_DISTINCT {
                    let shown = (!text.is_null(i)).then(|| text.value(i).to_string());
                    counts.insert(k, (shown, 1));
                }
            }
            Ok(())
        })?;
        let (mut min, mut max) = (None, None);
        if !ends.is_empty() {
            let arrays: Vec<&dyn Array> = ends.iter().map(|(a, _)| a.as_ref()).collect();
            let all = compute::concat(&arrays)?;
            let order = compute::sort_to_indices(all.as_ref(), None, None)?;
            min = order.values().first().map(|&i| ends[i as usize].1.clone());
            max = order.values().last().map(|&i| ends[i as usize].1.clone());
        }
        let distinct = counts.keys().filter(|k| k.is_some()).count() as u64;
        let mut top: Vec<(Option<String>, u64)> = counts.into_values().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let top = top.into_iter().take(10).map(|(value, count)| TopValue { value, count }).collect();
        Ok(ColumnStats { column, count, nulls, distinct, min, max, top })
    }
}
