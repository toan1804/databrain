//! Sorts and filters of a result on disk apply to its first rows only (a
//! window: the rows the grid has loaded, `ViewSpec::first_rows`), read into
//! memory once and sorted and filtered there by the in-memory code. Find,
//! statistics and export of the plain result read every row, file by file:
//! the rows are never all in memory.
//!
//! Long work (reading the window, find, statistics, streaming every row)
//! reads a snapshot of the files ([`Files`]) without holding the result's
//! lock, so the grid, the outputs list and new queries never wait on it. The
//! free functions here take the shared result; the `ResultSet` methods of the
//! same name do the same work under the lock (callers that already hold it).

use std::collections::HashMap;
use std::sync::Arc;

use databrain_connector_core::arrow::array::{Array, ArrayRef, BinaryArray, RecordBatch};
use databrain_connector_core::arrow::compute;
use databrain_connector_core::arrow::datatypes::SchemaRef;
use parking_lot::Mutex;

use crate::display::{self, is_decimal_text};
use crate::spill::{CHUNK_ROWS, Files};
use crate::view::{self, DisplayCache, ViewSpec};
use crate::{CellMatch, ColumnStats, Error, FindResult, Result, ResultSet, TopValue, decimal};

/// Distinct values counted for column statistics of a result on disk.
const MAX_DISTINCT: usize = 1_000_000;

/// Memory a window may take at most (fewer rows than asked past it).
pub const WINDOW_BYTES: usize = 256 << 20;

/// The first rows of a result on disk, in memory, that its sorts and
/// filters apply to.
pub(crate) struct Window {
    /// Rows asked for ([`asked`]); `set` has fewer when the result has
    /// fewer or they passed [`WINDOW_BYTES`].
    asked: usize,
    pub(crate) set: Box<ResultSet>,
}

/// Rows of the window `spec` asks for (all, up to [`WINDOW_BYTES`], when
/// it doesn't say).
fn asked(spec: &ViewSpec) -> usize {
    spec.first_rows.map_or(usize::MAX, |n| n.max(1))
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
    fn has_window(&self, spec: &ViewSpec) -> bool {
        self.window.as_ref().is_some_and(|w| w.asked == asked(spec))
    }

    /// Reading `spec` needs no window to be read (in memory, the plain
    /// result, or the window already there).
    fn view_ready(&self, spec: &ViewSpec) -> bool {
        !self.is_on_disk() || spec.is_identity() || self.has_window(spec)
    }

    fn window_plan(&self) -> Result<(Files, SchemaRef, String)> {
        if !self.complete {
            return Err(Error::OnDisk("Sorting and filtering while rows are still loading"));
        }
        let sp = self.spill.as_ref().ok_or(Error::OnDisk("This"))?;
        Ok((sp.snapshot(), self.schema.clone(), self.id.clone()))
    }

    /// The window `spec` applies to (a result on disk, a sort or filter),
    /// read under the lock if it isn't there (see [`prepare_view`] to read
    /// it without).
    pub(crate) fn window(&mut self, spec: &ViewSpec) -> Result<&mut ResultSet> {
        if !self.has_window(spec) {
            let (files, schema, id) = self.window_plan()?;
            self.window = None;
            let set = read_window(&files, schema, id, asked(spec))?;
            self.window = Some(Window { asked: asked(spec), set: Box::new(set) });
        }
        Ok(&mut self.window.as_mut().expect("just read").set)
    }

    /// Every row of a result on disk, a batch at a time.
    pub(crate) fn disk_for_each(&mut self, chunk: usize, f: &mut dyn FnMut(RecordBatch) -> Result<()>) -> Result<()> {
        let files = self.spill.as_ref().ok_or(Error::OnDisk("This"))?.snapshot();
        files.for_each(chunk, f)
    }

    pub(crate) fn disk_find(&mut self, query: &str, columns: Option<&[usize]>, limit: usize, whole_word: bool) -> Result<FindResult> {
        let files = self.spill.as_ref().ok_or(Error::OnDisk("This"))?.snapshot();
        find_in(&files, self.schema.fields().len(), query, columns, limit, whole_word)
    }

    pub(crate) fn disk_column_stats(&mut self, column: usize) -> Result<ColumnStats> {
        let files = self.spill.as_ref().ok_or(Error::OnDisk("This"))?.snapshot();
        stats_in(&files, &self.schema.clone(), column)
    }
}

/// The first `rows` rows of `files` (fewer past [`WINDOW_BYTES`]) as a
/// complete result in memory: row `i` of it is row `i` of the result.
fn read_window(files: &Files, schema: SchemaRef, id: String, rows: usize) -> Result<ResultSet> {
    let mut set = ResultSet::new(id, schema);
    let (mut have, mut bytes) = (0usize, 0usize);
    let r = files.for_each(CHUNK_ROWS, &mut |b| {
        let n = b.num_rows();
        let size = b.get_array_memory_size().max(1);
        // Rows of this batch that fit: all, or a share by size (at least one row overall).
        let mut take = n.min(rows - have);
        if bytes + size > WINDOW_BYTES {
            let room = WINDOW_BYTES.saturating_sub(bytes);
            take = take.min((n as u128 * room as u128 / size as u128) as usize);
            if have + take == 0 {
                take = 1;
            }
        }
        if take > 0 {
            set.push(b.slice(0, take));
            have += take;
            bytes += size;
        }
        if have >= rows || take < n {
            return Err(stop());
        }
        Ok(())
    });
    stopped(r)?;
    set.finish(false);
    // One copy of the rows, not the files' whole chunks they were sliced from.
    set.combined()?;
    Ok(set)
}

/// Read the window `spec` applies to for a result on disk (once), without
/// holding the result's lock; one read at a time per result. Nothing to do
/// in memory or for the plain result.
pub fn prepare_view(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec) -> Result<()> {
    if spec.is_identity() {
        return Ok(());
    }
    let build = {
        let g = rs.lock();
        if g.view_ready(spec) {
            return Ok(());
        }
        g.view_build.clone()
    };
    let _one = build.lock();
    let (files, schema, id) = {
        let mut g = rs.lock();
        if g.view_ready(spec) {
            return Ok(());
        }
        // The previous window goes first.
        g.window = None;
        g.window_plan()?
    };
    let set = read_window(&files, schema, id, asked(spec))?;
    rs.lock().window = Some(Window { asked: asked(spec), set: Box::new(set) });
    Ok(())
}

/// Run `f` on the result with the window of `spec` ready, read without the
/// lock. A request for another window can replace it in between: then it is
/// read again (a few times) before `f` would read it under the lock.
pub fn with_view<T>(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec, f: impl FnOnce(&mut ResultSet) -> Result<T>) -> Result<T> {
    for _ in 0..3 {
        prepare_view(rs, spec)?;
        let mut g = rs.lock();
        if g.view_ready(spec) {
            return f(&mut g);
        }
    }
    f(&mut rs.lock())
}

/// The files of a result on disk to read without the lock, for the plain
/// result (None: in memory, or a sort or filter, which reads the window).
fn snapshot(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec) -> Option<(Files, SchemaRef)> {
    let g = rs.lock();
    if !spec.is_identity() {
        return None;
    }
    g.spill.as_ref().map(|sp| (sp.snapshot(), g.schema()))
}

/// Every row of the view, a batch at a time (the result's own types). The
/// files of a result on disk are read without holding its lock.
pub fn for_each_batch(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec, chunk: usize, f: &mut dyn FnMut(RecordBatch) -> Result<()>) -> Result<()> {
    match snapshot(rs, spec) {
        Some((files, _)) => files.for_each(chunk, f),
        None => {
            // In memory: the batches are taken under the lock, used after it.
            let batches = with_view(rs, spec, |g| g.view_batches(spec, chunk))?;
            batches.into_iter().try_for_each(f)
        }
    }
}

/// [`ResultSet::find_with`]; the files of a result on disk are read without its lock.
pub fn find(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec, query: &str, columns: Option<&[usize]>, limit: usize, whole_word: bool) -> Result<FindResult> {
    match snapshot(rs, spec) {
        Some((files, schema)) => find_in(&files, schema.fields().len(), query, columns, limit, whole_word),
        None => with_view(rs, spec, |g| g.find_with(spec, query, columns, limit, whole_word)),
    }
}

/// [`ResultSet::column_stats`]; the files of a result on disk are read without its lock.
pub fn column_stats(rs: &Arc<Mutex<ResultSet>>, spec: &ViewSpec, column: usize) -> Result<ColumnStats> {
    match snapshot(rs, spec) {
        Some((files, schema)) => {
            if column >= schema.fields().len() {
                return Err(Error::InvalidColumn(column));
            }
            stats_in(&files, &schema, column)
        }
        None => with_view(rs, spec, |g| g.column_stats(spec, column)),
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
