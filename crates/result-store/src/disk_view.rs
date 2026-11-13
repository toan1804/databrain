//! Views (filters, quick filter, sort), find and column statistics of a
//! result on disk, file by file: the rows are never all in memory. Filters
//! and text matching are the in-memory code run on each file (same answers);
//! sorting goes to the [`DiskSorter`] (DuckDB, out of memory) with the keys
//! the in-memory sort compares, and the sorted rows are kept on disk too.
//!
//! Long work (building a view, find, statistics, streaming every row) reads
//! a snapshot of the files ([`Files`]) without holding the result's lock, so
//! the grid, the outputs list and new queries never wait on it. The free
//! functions here take the shared result; the `ResultSet` methods of the
//! same name do the same work under the lock (callers that already hold it).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use databrain_connector_core::arrow::array::{Array, ArrayRef, AsArray, BinaryArray, RecordBatch, UInt32Array, UInt64Array};
use databrain_connector_core::arrow::compute;
use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit, UInt64Type};
use parking_lot::Mutex;

use crate::display::{self, is_decimal_text};
use crate::spill::{CHUNK_ROWS, Files, Spill};
use crate::view::{self, DisplayCache, ViewSpec};
use crate::{CellMatch, ColumnStats, DiskSorter, DiskView, Error, FindResult, Result, ResultSet, SortJob, TopValue, decimal};

/// Distinct values counted for column statistics of a result on disk.
const MAX_DISTINCT: usize = 1_000_000;

/// The row's place in the result, in a view's files (and the sort's keys).
pub const ROW_ID: &str = "__databrain_row_id";

static VIEW_SEQ: AtomicU64 = AtomicU64::new(0);

/// Free disk space where results spill, and the floor to keep.
#[derive(Clone)]
pub struct SpaceCheck {
    pub free: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
    pub floor: u64,
}

/// `schema` plus the [`ROW_ID`] column.
fn with_row_id(schema: &SchemaRef) -> SchemaRef {
    let mut fields: Vec<Field> = schema.fields().iter().map(|f| f.as_ref().clone()).collect();
    fields.push(Field::new(ROW_ID, DataType::UInt64, false));
    Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

/// Split the [`ROW_ID`] column off a view batch.
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

/// Types DuckDB gives back as they were (a sorted view goes through it):
/// not 256-bit decimals (back as DOUBLE), nanosecond times, intervals,
/// durations or unions.
fn survives_duckdb(dt: &DataType) -> bool {
    match dt {
        DataType::Decimal256(..) | DataType::Time64(TimeUnit::Nanosecond) | DataType::Interval(_) | DataType::Duration(_) | DataType::Union(..) => false,
        DataType::List(f) | DataType::LargeList(f) | DataType::FixedSizeList(f, _) | DataType::Map(f, _) => survives_duckdb(f.data_type()),
        DataType::Struct(fs) => fs.iter().all(|f| survives_duckdb(f.data_type())),
        _ => true,
    }
}

/// What a view of a result on disk is built from, taken under the lock.
pub(crate) struct Plan {
    data: Files,
    starts: Vec<(PathBuf, usize)>,
    schema: SchemaRef,
    dir: PathBuf,
    sorter: Option<Arc<dyn DiskSorter>>,
    space: Option<SpaceCheck>,
    disk_bytes: u64,
    rows_per_group: usize,
}

impl ResultSet {
    fn has_view(&self, spec: &ViewSpec) -> bool {
        self.disk_view.as_ref().is_some_and(|v| &v.spec == spec)
    }

    pub(crate) fn view_plan(&self) -> Result<Plan> {
        if !self.complete {
            return Err(Error::OnDisk("Sorting and filtering while rows are still loading"));
        }
        let sp = self.spill.as_ref().ok_or(Error::OnDisk("This"))?;
        Ok(Plan {
            data: sp.snapshot(),
            starts: sp.file_starts(),
            schema: self.schema.clone(),
            dir: sp.dir().join(format!("view-{}", VIEW_SEQ.fetch_add(1, Ordering::Relaxed))),
            sorter: self.sorter.clone(),
            space: self.space.clone(),
            disk_bytes: sp.disk_bytes(),
            rows_per_group: sp.rows_per_group(),
        })
    }

    fn install_view(&mut self, spec: &ViewSpec, rows: Spill) {
        self.disk_view = Some(DiskView { spec: spec.clone(), rows });
    }

    /// The view of a result on disk for `spec` (not the identity), built
    /// once per spec under the lock (see [`prepare_view`] to build it
    /// without): a [`Spill`] of the kept rows in order, plus [`ROW_ID`].
    pub(crate) fn disk_view(&mut self, spec: &ViewSpec) -> Result<&mut Spill> {
        if !self.has_view(spec) {
            let plan = self.view_plan()?;
            // The previous view's files go first.
            self.disk_view = None;
            let rows = build_view(plan, spec)?;
            self.install_view(spec, rows);
        }
        Ok(&mut self.disk_view.as_mut().expect("just built").rows)
    }

    /// The rows of `spec` on disk as a snapshot, and whether they carry
    /// [`ROW_ID`] (a view).
    pub(crate) fn disk_snapshot(&mut self, spec: &ViewSpec) -> Result<(Files, bool)> {
        if spec.is_identity() {
            return Ok((self.spill.as_ref().ok_or(Error::OnDisk("This"))?.snapshot(), false));
        }
        Ok((self.disk_view(spec)?.snapshot(), true))
    }

    /// Every view batch of a result on disk, without [`ROW_ID`].
    pub(crate) fn disk_for_each(&mut self, spec: &ViewSpec, chunk: usize, f: &mut dyn FnMut(RecordBatch) -> Result<()>) -> Result<()> {
        let (files, ids) = self.disk_snapshot(spec)?;
        each(&files, ids, &self.schema.clone(), chunk, f)
    }

    pub(crate) fn disk_find(&mut self, spec: &ViewSpec, query: &str, columns: Option<&[usize]>, limit: usize, whole_word: bool) -> Result<FindResult> {
        let (files, _) = self.disk_snapshot(spec)?;
        find_in(&files, self.schema.fields().len(), query, columns, limit, whole_word)
    }

    pub(crate) fn disk_column_stats(&mut self, spec: &ViewSpec, column: usize) -> Result<ColumnStats> {
        let (files, _) = self.disk_snapshot(spec)?;
        stats_in(&files, &self.schema.clone(), column)
    }
}

fn each(files: &Files, ids: bool, schema: &SchemaRef, chunk: usize, f: &mut dyn FnMut(RecordBatch) -> Result<()>) -> Result<()> {
    files.for_each(chunk, &mut |b| if ids { f(split_ids(&b, schema)?.0) } else { f(b) })
}

/// Build a view (out of any lock): filters file by file; a sort through the
/// sorter. Refused when the disk lacks room for the copy it writes.
fn build_view(plan: Plan, spec: &ViewSpec) -> Result<Spill> {
    if let Some(sc) = &plan.space
        && let Some(free) = (sc.free)()
    {
        // A copy of the rows, and DuckDB's own temporary files for a sort.
        let need = sc.floor + plan.disk_bytes * if spec.sort.is_empty() { 1 } else { 2 };
        if free < need {
            return Err(Error::Spill(format!(
                "not enough free disk space to {} this result ({} MB free, about {} MB needed)",
                if spec.sort.is_empty() { "filter" } else { "sort" },
                free >> 20,
                need >> 20
            )));
        }
    }
    let ext = with_row_id(&plan.schema);
    let mut base = 0u64;
    if spec.sort.is_empty() {
        let mut out = Spill::create(plan.dir, ext.clone())?;
        plan.data.for_each(CHUNK_ROWS, &mut |b| {
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
        return Ok(out);
    }
    // A sort: DuckDB reads the files, so names it adds can't be columns,
    // and every type must come back as it went.
    for f in plan.schema.fields() {
        if [ROW_ID, "file_row_number", "__databrain_file"].contains(&f.name().as_str()) {
            return Err(Error::Spill(format!("a result with a column named {} can't be sorted on disk", f.name())));
        }
        if !survives_duckdb(f.data_type()) {
            return Err(Error::Spill(format!("a result with a {} column can't be sorted on disk", f.data_type())));
        }
    }
    let sorter = plan.sorter.ok_or(Error::OnDisk("Sorting (it needs DuckDB)"))?;
    let mut key_fields = vec![Field::new(ROW_ID, DataType::UInt64, false)];
    for (i, k) in spec.sort.iter().enumerate() {
        let f = plan.schema.fields().get(k.column).ok_or(Error::InvalidColumn(k.column))?;
        let dt = if is_decimal_text(f) { DataType::Binary } else { f.data_type().clone() };
        key_fields.push(Field::new(format!("k{i}"), dt, true));
    }
    let key_schema: SchemaRef = Arc::new(Schema::new(key_fields));
    // The view's folder: removed if anything below fails.
    let out = Spill::create(plan.dir.clone(), ext.clone())?;
    let mut keys = Spill::create(plan.dir.join("keys"), key_schema.clone())?;
    plan.data.for_each(CHUNK_ROWS, &mut |b| {
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
    if keys.rows() == 0 {
        // The filters keep nothing: an empty view (nothing to sort).
        return Ok(out);
    }
    let file = plan.dir.join("sorted.parquet");
    let job = SortJob {
        keys: keys.files(),
        data: plan.starts,
        descending: spec.sort.iter().map(|k| k.descending).collect(),
        out: file.clone(),
        row_group_rows: plan.rows_per_group,
    };
    sorter.sort(&job).map_err(Error::Spill)?;
    drop(keys);
    // `out` (empty) hands the folder over to the sorted file's rows.
    let rows = Spill::from_file(plan.dir, file, ext)?;
    std::mem::forget(out);
    Ok(rows)
}

/// Build the view of `spec` for a result on disk (once), reading its files
/// without holding the result's lock; one build at a time per result.
/// Nothing to do in memory or for the identity view.
pub fn prepare_view(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec) -> Result<()> {
    if spec.is_identity() {
        return Ok(());
    }
    let build = {
        let g = rs.lock();
        if !g.is_on_disk() || g.has_view(spec) {
            return Ok(());
        }
        g.view_build.clone()
    };
    let _one = build.lock();
    let plan = {
        let mut g = rs.lock();
        if g.has_view(spec) {
            return Ok(());
        }
        // The previous view's files go first.
        g.disk_view = None;
        g.view_plan()?
    };
    let rows = build_view(plan, spec)?;
    rs.lock().install_view(spec, rows);
    Ok(())
}

/// The rows of `spec` to read without the lock (None: the result is in memory).
fn snapshot(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec) -> Result<Option<(Files, bool, SchemaRef)>> {
    prepare_view(rs, spec)?;
    let mut g = rs.lock();
    if !g.is_on_disk() {
        return Ok(None);
    }
    let schema = g.schema();
    let (files, ids) = g.disk_snapshot(spec)?;
    Ok(Some((files, ids, schema)))
}

/// Every row of the view, a batch at a time (the result's own types). A
/// result on disk is read without holding its lock.
pub fn for_each_batch(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec, chunk: usize, f: &mut dyn FnMut(RecordBatch) -> Result<()>) -> Result<()> {
    match snapshot(rs, spec)? {
        Some((files, ids, schema)) => each(&files, ids, &schema, chunk, f),
        None => rs.lock().for_each_batch(spec, chunk, f),
    }
}

/// [`ResultSet::find_with`]; a result on disk is read without its lock.
pub fn find(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec, query: &str, columns: Option<&[usize]>, limit: usize, whole_word: bool) -> Result<FindResult> {
    match snapshot(rs, spec)? {
        Some((files, _, schema)) => find_in(&files, schema.fields().len(), query, columns, limit, whole_word),
        None => rs.lock().find_with(spec, query, columns, limit, whole_word),
    }
}

/// [`ResultSet::column_stats`]; a result on disk is read without its lock.
pub fn column_stats(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec, column: usize) -> Result<ColumnStats> {
    match snapshot(rs, spec)? {
        Some((files, _, schema)) => {
            if column >= schema.fields().len() {
                return Err(Error::InvalidColumn(column));
            }
            stats_in(&files, &schema, column)
        }
        None => rs.lock().column_stats(spec, column),
    }
}

/// Cells containing `query` (as in memory), file by file.
fn find_in(files: &Files, ncols: usize, query: &str, columns: Option<&[usize]>, limit: usize, whole_word: bool) -> Result<FindResult> {
    if query.is_empty() {
        return Ok(FindResult { matches: Vec::new(), truncated: false });
    }
    let cols: Vec<usize> = match columns {
        Some(c) if !c.is_empty() => (0..ncols).filter(|i| c.contains(i)).collect(),
        _ => (0..ncols).collect(),
    };
    let pattern = format!("%{}%", view::like_escape(query));
    let needle = query.to_lowercase();
    let (mut matches, mut truncated, mut at) = (Vec::new(), false, 0usize);
    let r = files.for_each(CHUNK_ROWS, &mut |b| {
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

/// Column statistics (as in memory), file by file; distinct values are
/// counted up to a million (`distinct_capped` past it).
fn stats_in(files: &Files, schema: &SchemaRef, column: usize) -> Result<ColumnStats> {
    let field = schema.field(column).clone();
    let decimal_text = is_decimal_text(&field);
    let (mut count, mut nulls, mut capped) = (0u64, 0u64, false);
    // Each file's smallest and largest value (as compared, and as shown).
    let mut ends: Vec<(ArrayRef, String)> = Vec::new();
    let mut counts: HashMap<Option<Vec<u8>>, (Option<String>, u64)> = HashMap::new();
    files.for_each(CHUNK_ROWS, &mut |b| {
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
            } else {
                capped = true;
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
    Ok(ColumnStats { column, count, nulls, distinct, distinct_capped: capped, min, max, top })
}
