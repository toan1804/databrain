//! In-memory store for query results with local view operations
//! (filter, quick filter, sort, find, column stats) and paging for the grid.

pub mod chart;
mod decimal;
mod disk_view;
pub use disk_view::{WINDOW_BYTES, column_stats, find, for_each_batch, prepare_view, with_view, with_window};
pub mod display;
pub mod spill;
pub mod view;
mod wide;

use std::collections::HashMap;
use std::sync::Arc;

use databrain_connector_core::arrow::array::{Array, ArrayRef, BinaryArray, RecordBatch, UInt32Array};
use databrain_connector_core::arrow::compute;
use databrain_connector_core::arrow::datatypes::{DataType, SchemaRef};
use databrain_connector_core::arrow::error::ArrowError;
use databrain_connector_core::arrow::util::display::ArrayFormatter;
use parking_lot::Mutex;
use serde::Serialize;

pub use chart::{Agg, ChartData, ChartSeries, ChartSpec};
pub use display::{ColumnMeta, TypeFamily, column_meta};
pub use view::{ColumnFilter, DisplayCache, FilterOp, SortKey, ViewSpec};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("result not found: {0}")]
    NotFound(String),
    #[error("invalid column index {0}")]
    InvalidColumn(usize),
    #[error("{0}")]
    InvalidFilterValue(String),
    #[error("arrow: {0}")]
    Arrow(#[from] ArrowError),
    /// Writing or reading the result's files on disk failed.
    #[error("result on disk: {0}")]
    Spill(String),
    /// Needs every row in memory, and the result is on disk.
    #[error("{0} isn't available yet for a result kept on disk (larger than memory)")]
    OnDisk(&'static str),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A page of display-ready cells for the grid.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct Page {
    /// Rows in the current view (after filters).
    pub view_rows: usize,
    /// Rows stored in the result (before filters).
    pub total_rows: usize,
    /// A sort or filter of a result on disk applies to its first rows only:
    /// this many (None: to every row).
    pub window_rows: Option<usize>,
    pub offset: usize,
    /// 0-based index of each row in the original result.
    pub row_ids: Vec<u32>,
    /// `rows[r][c]`; `None` is SQL NULL.
    pub rows: Vec<Vec<Option<String>>>,
}

/// `needle` occurs in `hay` with no letter, digit or `_` right before or
/// after it (both lower-cased).
pub fn contains_word(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut from = 0;
    while let Some(p) = hay[from..].find(needle) {
        let at = from + p;
        let before = hay[..at].chars().next_back();
        let after = hay[at + needle.len()..].chars().next();
        // A needle that itself starts/ends with punctuation needs no boundary there.
        let starts = !needle.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_');
        let ends = !needle.chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_');
        if (starts || !word(before)) && (ends || !word(after)) {
            return true;
        }
        from = at + needle.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Most distinct values counted for a selection summary.
const MAX_DISTINCT: usize = 100_000;

/// Count, distinct and (for numbers) sum / min / max / avg of selected cells.
#[derive(Debug, Clone, Default, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SelectionSummary {
    pub cells: usize,
    pub non_null: usize,
    pub distinct: usize,
    /// More distinct values than counted.
    pub distinct_capped: bool,
    /// Cells holding a number (numeric columns only).
    pub numbers: usize,
    pub sum: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub avg: Option<f64>,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct CellMatch {
    /// Row position within the view.
    pub row: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct FindResult {
    pub matches: Vec<CellMatch>,
    /// True when the match list was cut at the limit.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct TopValue {
    pub value: Option<String>,
    #[ts(as = "f64")]
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ColumnStats {
    pub column: usize,
    #[ts(as = "f64")]
    pub count: u64,
    #[ts(as = "f64")]
    pub nulls: u64,
    #[ts(as = "f64")]
    pub distinct: u64,
    pub min: Option<String>,
    pub max: Option<String>,
    pub top: Vec<TopValue>,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ResultInfo {
    pub id: String,
    pub columns: Vec<ColumnMeta>,
    pub total_rows: usize,
    pub complete: bool,
    /// Stopped at the row limit; more rows exist on the server.
    pub truncated: bool,
    /// Bytes in memory.
    pub bytes: usize,
    /// Rows past the memory a result may keep are on disk (read as they're
    /// shown); `disk_bytes` of files.
    pub on_disk: bool,
    #[ts(type = "number")]
    pub disk_bytes: u64,
}

struct ViewCache {
    spec: ViewSpec,
    rows: usize,
    indices: Arc<UInt32Array>,
}

/// One query's result. Batches are appended while the query streams.
pub struct ResultSet {
    id: String,
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    combined: Option<RecordBatch>,
    total_rows: usize,
    complete: bool,
    truncated: bool,
    view_cache: Option<ViewCache>,
    display_cache: DisplayCache,
    /// Bytes per text/binary column before 64-bit offsets are used (tests lower it).
    wide_limit: usize,
    /// Set once the result outgrew its memory: every row is there (the
    /// in-memory batches moved in), `batches` stays empty.
    spill: Option<spill::Spill>,
    /// Writing to disk failed: no rows were added after it.
    spill_error: Option<String>,
    /// The first rows a sort or filter of a result on disk applies to.
    window: Option<disk_view::Window>,
    /// One window read at a time (outside the lock: see [`prepare_view`]).
    view_build: Arc<Mutex<()>>,
}

impl ResultSet {
    pub fn new(id: impl Into<String>, schema: SchemaRef) -> Self {
        Self {
            id: id.into(),
            schema,
            batches: Vec::new(),
            combined: None,
            total_rows: 0,
            complete: false,
            truncated: false,
            view_cache: None,
            display_cache: DisplayCache::default(),
            wide_limit: display::MAX_SMALL_BYTES,
            spill: None,
            spill_error: None,
            window: None,
            view_build: Arc::new(Mutex::new(())),
        }
    }

    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    pub fn total_rows(&self) -> usize {
        self.total_rows
    }

    pub fn push(&mut self, batch: RecordBatch) {
        if batch.num_rows() == 0 || self.spill_error.is_some() {
            return;
        }
        if let Some(sp) = &mut self.spill {
            // A failed write drops the rows waiting for a file: the count
            // follows, so every reader sees the same rows.
            if let Err(e) = sp.push(batch) {
                self.spill_error = Some(e.to_string());
            }
            self.total_rows = sp.rows();
            return;
        }
        self.total_rows += batch.num_rows();
        self.batches.push(batch);
        self.combined = None;
    }

    pub fn finish(&mut self, truncated: bool) {
        if let Some(sp) = &mut self.spill {
            if let Err(e) = sp.finish() {
                self.spill_error = Some(e.to_string());
            }
            self.total_rows = sp.rows();
        }
        self.complete = true;
        self.truncated = truncated;
    }

    /// Keep the rows on disk in `dir` from now on: the rows so far move
    /// there, and memory holds only a few pages. `sample` (the next rows)
    /// is tried first: when these rows can't be written as Parquet, or a
    /// write fails, nothing changes (the rows stay in memory) and the error
    /// is returned.
    pub fn spill_to(&mut self, dir: std::path::PathBuf, sample: &RecordBatch) -> Result<()> {
        if self.spill.is_some() {
            return Ok(());
        }
        let probe = self.batches.first().unwrap_or(sample);
        if !spill::can_write(probe) || !spill::can_write(sample) {
            return Err(Error::Spill("these column types can't be written to disk".into()));
        }
        let mut sp = spill::Spill::create(dir, self.schema.clone())?;
        // The narrow batches, not a combined one with 64-bit offsets; the
        // in-memory rows are let go only once all of them are written.
        let batches =
            if self.combined.is_some() && self.batches.len() == 1 { self.view_batches(&ViewSpec::default(), spill::CHUNK_ROWS)? } else { self.batches.clone() };
        for b in batches {
            sp.push(b)?;
        }
        self.batches.clear();
        self.combined = None;
        self.view_cache = None;
        self.display_cache = DisplayCache::default();
        self.total_rows = sp.rows();
        self.spill = Some(sp);
        Ok(())
    }

    pub fn is_on_disk(&self) -> bool {
        self.spill.is_some()
    }

    /// Why rows stopped being added (writing to disk failed).
    pub fn spill_error(&self) -> Option<&str> {
        self.spill_error.as_deref()
    }

    /// The schema SQL sees for a result on disk (see [`spill::Spill::sql_schema`]).
    pub fn disk_sql_schema(&self) -> Option<SchemaRef> {
        self.spill.as_ref().map(spill::Spill::sql_schema)
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// The result's files on disk (complete files only), in row order.
    pub fn spill_files(&self) -> Option<Vec<std::path::PathBuf>> {
        self.spill.as_ref().map(spill::Spill::files)
    }

    pub fn memory_bytes(&self) -> usize {
        if let Some(sp) = &self.spill {
            return sp.memory_bytes() + self.window.as_ref().map_or(0, |w| w.set.memory_bytes());
        }
        match &self.combined {
            Some(c) => databrain_connector_core::memory_size([c]),
            None => databrain_connector_core::memory_size(&self.batches),
        }
    }

    /// Every row of the view, a batch at a time (at most `chunk` rows, the
    /// result's own types), without keeping them all: for exports and loads
    /// of results on disk too (a sort or filter of one: of its window).
    pub fn for_each_batch(&mut self, spec: &ViewSpec, chunk: usize, f: &mut dyn FnMut(RecordBatch) -> Result<()>) -> Result<()> {
        if self.spill.is_some() {
            if spec.is_identity() {
                return self.disk_for_each(chunk, f);
            }
            return self.window(spec)?.for_each_batch(spec, chunk, f);
        }
        for b in self.view_batches(spec, chunk)? {
            f(b)?;
        }
        Ok(())
    }

    pub fn info(&self) -> ResultInfo {
        ResultInfo {
            id: self.id.clone(),
            columns: self.schema.fields().iter().map(|f| column_meta(f)).collect(),
            total_rows: self.total_rows,
            complete: self.complete,
            truncated: self.truncated,
            bytes: self.memory_bytes(),
            on_disk: self.spill.is_some(),
            disk_bytes: self.spill.as_ref().map_or(0, spill::Spill::disk_bytes),
        }
    }

    /// All rows as one batch (concatenated lazily and cached). Text/binary
    /// columns with more than 2 GiB of values come back with 64-bit offsets
    /// (`LargeUtf8`/`LargeBinary`); use [`ResultSet::view_batches`] for
    /// batches with the result's own types.
    pub fn combined(&mut self) -> Result<RecordBatch> {
        if self.spill.is_some() {
            return Err(Error::OnDisk("This"));
        }
        if let Some(c) = &self.combined {
            return Ok(c.clone());
        }
        let c = if self.batches.len() == 1 { self.batches[0].clone() } else { wide::concat_wide(&self.schema, &self.batches, self.wide_limit)? };
        // Once complete, keep only the combined copy to halve memory use.
        if self.complete {
            self.batches = vec![c.clone()];
        }
        self.combined = Some(c.clone());
        Ok(c)
    }

    /// Row indices for `spec`, cached until the spec or data changes.
    pub fn view_indices(&mut self, spec: &ViewSpec) -> Result<Option<Arc<UInt32Array>>> {
        if spec.is_identity() {
            return Ok(None);
        }
        if self.spill.is_some() {
            return self.window(spec)?.view_indices(spec);
        }
        if let Some(c) = &self.view_cache
            && &c.spec == spec
            && c.rows == self.total_rows
        {
            return Ok(Some(c.indices.clone()));
        }
        let batch = self.combined()?;
        let idx = Arc::new(view::compute_view(&batch, spec, &mut self.display_cache)?);
        self.view_cache = Some(ViewCache { spec: spec.clone(), rows: self.total_rows, indices: idx.clone() });
        Ok(Some(idx))
    }

    pub fn view_len(&mut self, spec: &ViewSpec) -> Result<usize> {
        if self.spill.is_some() && !spec.is_identity() {
            return self.window(spec)?.view_len(spec);
        }
        Ok(match self.view_indices(spec)? {
            Some(i) => i.len(),
            None => self.total_rows,
        })
    }

    /// Summary of a selection: rows `[offset, offset+limit)` of the view, in
    /// `columns` (empty = all). Numbers (also exact decimals) are summed.
    pub fn summarize(&mut self, spec: &ViewSpec, columns: &[usize], offset: usize, limit: usize) -> Result<SelectionSummary> {
        let (batch, _) = self.view_slice(spec, offset, limit)?;
        let cols: Vec<usize> =
            if columns.is_empty() { (0..batch.num_columns()).collect() } else { columns.iter().copied().filter(|&c| c < batch.num_columns()).collect() };
        let opts = display::format_options();
        let mut s = SelectionSummary::default();
        let mut distinct = std::collections::HashSet::new();
        // Floats are summed as f64; exact numbers (decimals, integers) exactly.
        let (mut sum, mut min, mut max) = (0f64, f64::INFINITY, f64::NEG_INFINITY);
        let (mut exact_sum, mut exact_range) = (decimal::ExactSum::default(), decimal::ExactRange::default());
        for &c in &cols {
            let a = batch.column(c);
            let numeric = display::family(batch.schema().field(c)) == display::TypeFamily::Number;
            let float = matches!(a.data_type(), DataType::Float16 | DataType::Float32 | DataType::Float64);
            let f = ArrayFormatter::try_new(a.as_ref(), &opts)?;
            for r in 0..batch.num_rows() {
                s.cells += 1;
                if a.is_null(r) {
                    continue;
                }
                s.non_null += 1;
                let text = f.value(r).to_string();
                // Distinct by value for exact numbers (1.50 = 1.5), by text otherwise.
                let mut key = None;
                if numeric && float {
                    if let Ok(v) = text.parse::<f64>()
                        && v.is_finite()
                    {
                        s.numbers += 1;
                        sum += v;
                        min = min.min(v);
                        max = max.max(v);
                    }
                } else if numeric && let Some(n) = decimal::Num::parse(&text) {
                    if n.is_finite() {
                        s.numbers += 1;
                        exact_sum.add(&n);
                        exact_range.add(&n);
                    }
                    key = Some([b"n".as_slice(), &n.key()].concat());
                }
                let key = key.unwrap_or_else(|| [b"t".as_slice(), text.as_bytes()].concat());
                if distinct.len() < MAX_DISTINCT {
                    distinct.insert(key);
                } else {
                    s.distinct_capped = true;
                }
            }
        }
        s.distinct = distinct.len();
        if s.numbers > 0 {
            if let (Some((_, lo)), Some((_, hi))) = (&exact_range.min, &exact_range.max) {
                sum += exact_sum.to_f64();
                min = min.min(lo.to_f64());
                max = max.max(hi.to_f64());
            }
            s.sum = Some(sum);
            s.min = Some(min);
            s.max = Some(max);
            s.avg = Some(sum / s.numbers as f64);
        }
        Ok(s)
    }

    /// Rows `[offset, offset+limit)` of the view as a batch plus original ids.
    pub fn view_slice(&mut self, spec: &ViewSpec, offset: usize, limit: usize) -> Result<(RecordBatch, Vec<u32>)> {
        if self.spill.is_some() && !spec.is_identity() {
            return self.window(spec)?.view_slice(spec, offset, limit);
        }
        if let Some(sp) = &mut self.spill {
            let b = sp.slice(offset, limit)?;
            let start = offset.min(sp.rows());
            return Ok((b.clone(), (start as u32..(start + b.num_rows()) as u32).collect()));
        }
        let batch = self.combined()?;
        let (b, ids) = match self.view_indices(spec)? {
            None => {
                let start = offset.min(batch.num_rows());
                let len = limit.min(batch.num_rows() - start);
                let ids = (start as u32..(start + len) as u32).collect();
                (batch.slice(start, len), ids)
            }
            Some(idx) => {
                let start = offset.min(idx.len());
                let len = limit.min(idx.len() - start);
                let part = idx.slice(start, len);
                let taken = compute::take_record_batch(&batch, &part)?;
                (taken, part.values().to_vec())
            }
        };
        // The result's own types again (fits unless the slice itself passes 2 GiB).
        let b = match wide::narrow(&self.schema, b.clone()) {
            Ok(n) => n,
            Err(_) => b,
        };
        Ok((b, ids))
    }

    pub fn page(&mut self, spec: &ViewSpec, offset: usize, limit: usize) -> Result<Page> {
        if self.spill.is_some() && !spec.is_identity() {
            let total_rows = self.total_rows;
            let w = self.window(spec)?;
            let window_rows = Some(w.total_rows);
            return Ok(Page { total_rows, window_rows, ..w.page(spec, offset, limit)? });
        }
        let view_rows = self.view_len(spec)?;
        let (batch, row_ids) = self.view_slice(spec, offset, limit)?;
        let opts = display::format_options();
        let formatters: Vec<ArrayFormatter<'_>> = batch.columns().iter().map(|c| ArrayFormatter::try_new(c.as_ref(), &opts)).collect::<Result<_, _>>()?;
        let rows = (0..batch.num_rows()).map(|r| formatters.iter().zip(batch.columns()).map(|(f, a)| display::cell(f, a, r)).collect()).collect();
        Ok(Page { view_rows, total_rows: self.total_rows, window_rows: None, offset, row_ids, rows })
    }

    /// Find cells containing `query` (case-insensitive) within the view.
    pub fn find(&mut self, spec: &ViewSpec, query: &str, limit: usize) -> Result<FindResult> {
        self.find_in(spec, query, None, limit)
    }

    /// Find `query` (case-insensitive substring of the displayed value),
    /// optionally only in the given columns.
    pub fn find_in(&mut self, spec: &ViewSpec, query: &str, columns: Option<&[usize]>, limit: usize) -> Result<FindResult> {
        self.find_with(spec, query, columns, limit, false)
    }

    /// [`ResultSet::find_in`]; `whole_word`: only where `query` is a whole
    /// word of the value (`id` finds `id` and `a id`, not `paid`).
    pub fn find_with(&mut self, spec: &ViewSpec, query: &str, columns: Option<&[usize]>, limit: usize, whole_word: bool) -> Result<FindResult> {
        let mut matches = Vec::new();
        if query.is_empty() || self.total_rows == 0 {
            return Ok(FindResult { matches, truncated: false });
        }
        if self.spill.is_some() {
            if spec.is_identity() {
                return self.disk_find(query, columns, limit, whole_word);
            }
            return self.window(spec)?.find_with(spec, query, columns, limit, whole_word);
        }
        let batch = self.combined()?;
        let idx = self.view_indices(spec)?;
        let pattern = format!("%{}%", view::like_escape(query));
        let cols: Vec<usize> = match columns {
            Some(c) if !c.is_empty() => {
                let mut c: Vec<usize> = c.iter().copied().filter(|&i| i < batch.num_columns()).collect();
                c.sort_unstable();
                c.dedup();
                c
            }
            _ => (0..batch.num_columns()).collect(),
        };
        let mut masks = Vec::with_capacity(batch.num_columns());
        for c in 0..batch.num_columns() {
            if cols.binary_search(&c).is_ok() {
                let text = self.display_cache.get(&batch, c)?;
                let mut m = text.ilike(&pattern, false)?;
                if whole_word {
                    // The substring match narrowed down; keep whole-word hits.
                    let needle = query.to_lowercase();
                    m = (0..m.len()).map(|r| Some(m.is_valid(r) && m.value(r) && contains_word(&text.value(r).to_lowercase(), &needle))).collect();
                }
                masks.push(Some(m));
            } else {
                masks.push(None);
            }
        }
        let hit = |row: usize, col: usize| masks[col].as_ref().is_some_and(|m| m.is_valid(row) && m.value(row));
        let n_view = idx.as_ref().map(|i| i.len()).unwrap_or(batch.num_rows());
        for vr in 0..n_view {
            let row = idx.as_ref().map(|i| i.value(vr) as usize).unwrap_or(vr);
            for c in 0..batch.num_columns() {
                if hit(row, c) {
                    if matches.len() >= limit {
                        return Ok(FindResult { matches, truncated: true });
                    }
                    matches.push(CellMatch { row: vr as u32, col: c as u32 });
                }
            }
        }
        Ok(FindResult { matches, truncated: false })
    }

    /// Statistics of one column over the current view.
    pub fn column_stats(&mut self, spec: &ViewSpec, column: usize) -> Result<ColumnStats> {
        if column >= self.schema.fields().len() {
            return Err(Error::InvalidColumn(column));
        }
        if self.spill.is_some() {
            // The rows in memory only (the window), never the files.
            return self.window(spec)?.column_stats(spec, column);
        }
        let n = self.view_len(spec)?;
        let (batch, _) = self.view_slice(spec, 0, n)?;
        let col = batch.column(column).clone();
        let field = self.schema.field(column).clone();
        let nulls = col.null_count() as u64;

        // min / max via sort (works for every orderable type; exact decimals by key).
        let decimal_text = display::is_decimal_text(&field);
        let sortable = if decimal_text { Arc::new(decimal::keys(col.as_ref())?) as ArrayRef } else { col.clone() };
        let text = display::to_display(col.as_ref())?;
        let (mut min, mut max) = (None, None);
        if col.len() > nulls as usize
            && let Ok(order) = compute::sort_to_indices(sortable.as_ref(), Some(compute::SortOptions { descending: false, nulls_first: false }), None)
        {
            let non_null: Vec<u32> = order.values().iter().copied().filter(|i| !sortable.is_null(*i as usize)).collect();
            if let (Some(first), Some(last)) = (non_null.first(), non_null.last()) {
                min = Some(text.value(*first as usize).to_string());
                max = Some(text.value(*last as usize).to_string());
            }
        }

        // Grouped by value for exact decimals (1.50 = 1.5; shown as first seen), by text otherwise.
        let keys = decimal_text.then(|| sortable.as_any().downcast_ref::<BinaryArray>().cloned()).flatten();
        let mut counts: HashMap<Option<&[u8]>, (usize, u64)> = HashMap::new();
        for i in 0..text.len() {
            let k = if text.is_null(i) {
                None
            } else {
                match &keys {
                    Some(k) if k.is_valid(i) => Some(k.value(i)),
                    _ => Some(text.value(i).as_bytes()),
                }
            };
            counts.entry(k).or_insert((i, 0)).1 += 1;
        }
        let distinct = counts.keys().filter(|k| k.is_some()).count() as u64;
        let value = |i: usize| (!text.is_null(i)).then(|| text.value(i));
        let mut top: Vec<(Option<&str>, u64)> = counts.into_values().map(|(i, n)| (value(i), n)).collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let top = top.into_iter().take(10).map(|(v, count)| TopValue { value: v.map(|s| s.to_string()), count }).collect();

        Ok(ColumnStats { column, count: col.len() as u64, nulls, distinct, min, max, top })
    }

    /// The view as a sequence of batches (for export, outputs), at most
    /// `chunk` rows each, always with the result's own column types: a chunk
    /// whose text passes 2 GiB is split further.
    pub fn view_batches(&mut self, spec: &ViewSpec, chunk: usize) -> Result<Vec<RecordBatch>> {
        if self.spill.is_some() {
            if !spec.is_identity() {
                return self.window(spec)?.view_batches(spec, chunk);
            }
            // Every row in memory is what spilling avoids: use for_each_batch.
            return Err(Error::OnDisk("Reading every row at once"));
        }
        let n = self.view_len(spec)?;
        let chunk = chunk.max(1);
        let mut out = Vec::with_capacity(n.div_ceil(chunk));
        let mut off = 0;
        while off < n {
            let len = chunk.min(n - off);
            self.push_narrow(spec, off, len, &mut out)?;
            off += len;
        }
        Ok(out)
    }

    fn push_narrow(&mut self, spec: &ViewSpec, off: usize, len: usize, out: &mut Vec<RecordBatch>) -> Result<()> {
        let (b, _) = self.view_slice(spec, off, len)?;
        let own = b.schema().fields().iter().zip(self.schema.fields()).all(|(a, f)| a.data_type() == f.data_type());
        if own {
            out.push(b);
            return Ok(());
        }
        if len <= 1 {
            // One value over 2 GiB: only the wide type can hold it.
            out.push(b);
            return Ok(());
        }
        let half = len / 2;
        self.push_narrow(spec, off, half, out)?;
        self.push_narrow(spec, off + half, len - half, out)
    }
}

/// Registry of live results, keyed by id.
#[derive(Default)]
pub struct ResultStore {
    results: Mutex<HashMap<String, Arc<Mutex<ResultSet>>>>,
    /// Folder for results kept on disk (None: results never spill).
    spill_root: Mutex<Option<std::path::PathBuf>>,
    /// Held while this process uses its spill folder (see [`Self::set_spill_root`]).
    spill_lock: Mutex<Option<std::fs::File>>,
}

impl ResultStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep results that outgrow memory under `base`, in a folder of this
    /// process's own (the app, the jobs runner and the MCP server can run at
    /// once). Each process holds a lock on its folder; folders nobody holds
    /// (left by a process that quit or crashed) are deleted: results on disk
    /// don't outlive their process.
    pub fn set_spill_root(&self, base: std::path::PathBuf) {
        let _ = std::fs::create_dir_all(&base);
        if let Ok(entries) = std::fs::read_dir(&base) {
            for e in entries.flatten() {
                let dir = e.path();
                if !dir.is_dir() {
                    continue;
                }
                let held = std::fs::OpenOptions::new().write(true).open(dir.join("lock")).is_ok_and(|f| f.try_lock().is_err());
                // A process starting right now may not have locked its folder
                // yet: only folders untouched for a minute are stale.
                let fresh = |p: &std::path::Path| {
                    std::fs::metadata(p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age < std::time::Duration::from_secs(60))
                };
                if !held && !fresh(&dir) && !fresh(&dir.join("lock")) {
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let mine = base.join(format!("{}-{nanos}", std::process::id()));
        let lock = std::fs::create_dir_all(&mine).and_then(|_| std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(mine.join("lock")));
        match lock.and_then(|f| f.lock().map(|_| f)) {
            Ok(f) => {
                *self.spill_lock.lock() = Some(f);
                *self.spill_root.lock() = Some(mine);
            }
            // No folder of our own: results don't spill (they stop at memory).
            Err(_) => *self.spill_root.lock() = None,
        }
    }

    pub fn spill_root(&self) -> Option<std::path::PathBuf> {
        self.spill_root.lock().clone()
    }

    /// The folder for result `id`'s files (None when results don't spill).
    pub fn spill_dir(&self, id: &str) -> Option<std::path::PathBuf> {
        let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        self.spill_root.lock().as_ref().map(|r| r.join(safe))
    }

    /// Insert a complete result (e.g. restored from a snapshot).
    pub fn insert_complete(&self, id: impl Into<String>, schema: SchemaRef, batches: Vec<RecordBatch>, truncated: bool) -> Arc<Mutex<ResultSet>> {
        let rs = self.create(id, schema);
        {
            let mut g = rs.lock();
            for b in batches {
                g.push(b);
            }
            g.finish(truncated);
        }
        rs
    }

    pub fn contains(&self, id: &str) -> bool {
        self.results.lock().contains_key(id)
    }

    /// Bytes `id` keeps in memory. The store's map isn't held while the
    /// result's lock is waited for (a long export or sort holds it).
    pub fn bytes_of(&self, id: &str) -> usize {
        self.size_of(id).0
    }

    /// Bytes `id` keeps in memory, and whether its rows are on disk.
    pub fn size_of(&self, id: &str) -> (usize, bool) {
        let rs = self.results.lock().get(id).cloned();
        rs.map(|r| {
            let g = r.lock();
            (g.memory_bytes(), g.is_on_disk())
        })
        .unwrap_or((0, false))
    }

    pub fn create(&self, id: impl Into<String>, schema: SchemaRef) -> Arc<Mutex<ResultSet>> {
        let id = id.into();
        let rs = Arc::new(Mutex::new(ResultSet::new(id.clone(), schema)));
        self.results.lock().insert(id, rs.clone());
        rs
    }

    pub fn get(&self, id: &str) -> Result<Arc<Mutex<ResultSet>>> {
        self.results.lock().get(id).cloned().ok_or_else(|| Error::NotFound(id.to_string()))
    }

    pub fn remove(&self, id: &str) -> bool {
        self.results.lock().remove(id).is_some()
    }

    pub fn total_bytes(&self) -> usize {
        let all: Vec<_> = self.results.lock().values().cloned().collect();
        all.iter().map(|r| r.lock().memory_bytes()).sum()
    }
}

#[cfg(test)]
mod spill_tests {
    use super::*;
    use databrain_connector_core::arrow::array::{Int64Array, IntervalMonthDayNanoArray};
    use databrain_connector_core::arrow::datatypes::{Field, IntervalMonthDayNano, IntervalUnit, Schema};

    /// Rows whose types have no Parquet form (intervals) stay in memory:
    /// moving to disk is refused before anything moves, and every row is kept.
    #[test]
    fn rows_that_cant_be_written_stay_in_memory() {
        let schema: SchemaRef =
            Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false), Field::new("age", DataType::Interval(IntervalUnit::MonthDayNano), true)]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(0..40_000)),
                Arc::new(IntervalMonthDayNanoArray::from(vec![Some(IntervalMonthDayNano::new(1, 2, 3)); 40_000])),
            ],
        )
        .unwrap();
        let mut rs = ResultSet::new("r", schema);
        rs.push(b.clone());
        let dir = tempfile::tempdir().unwrap();
        assert!(rs.spill_to(dir.path().join("r"), &b).is_err());
        assert!(!rs.is_on_disk());
        assert_eq!(rs.total_rows(), 40_000);
        assert_eq!(rs.view_slice(&ViewSpec::default(), 39_999, 5).unwrap().0.num_rows(), 1, "every row still there");
    }

    /// A sort or filter of a result on disk applies to its first rows only
    /// (the rows the grid has loaded); the plain result, find and statistics
    /// without a filter, and every-row reads see every row.
    #[test]
    fn sorts_and_filters_on_disk_apply_to_the_first_rows() {
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]));
        let n = 100_000i64;
        let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from_iter_values(0..n))]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let rs = Arc::new(Mutex::new(ResultSet::new("r", schema)));
        {
            let mut g = rs.lock();
            g.push(b.slice(0, 1000));
            g.spill_to(dir.path().join("r"), &b).unwrap();
            g.push(b.slice(1000, n as usize - 1000));
            g.finish(false);
            assert!(g.is_on_disk());
        }
        let first = |p: &Page| p.rows.iter().map(|r| r[0].clone().unwrap()).collect::<Vec<_>>();
        let desc = |first_rows| ViewSpec { sort: vec![SortKey { column: 0, descending: true }], first_rows, ..Default::default() };

        // The first 1,000 rows, sorted: 999 first; rows past them aren't in the view.
        let p = with_view(&rs, &desc(Some(1000)), |g| g.page(&desc(Some(1000)), 0, 3)).unwrap();
        assert_eq!((p.view_rows, p.total_rows, p.window_rows), (1000, n as usize, Some(1000)));
        assert_eq!(first(&p), vec!["999", "998", "997"]);
        assert_eq!(p.row_ids, vec![999, 998, 997]);
        let past =
            ViewSpec { filters: vec![ColumnFilter { column: 0, op: FilterOp::Gte, value: "1000".into() }], first_rows: Some(1000), ..Default::default() };
        assert_eq!(with_view(&rs, &past, |g| g.view_len(&past)).unwrap(), 0, "a filter sees only the first rows");
        assert_eq!(find(&rs, &past, "5", None, 10_000, false).unwrap().matches.len(), 0);
        let stats = column_stats(&rs, &desc(Some(1000)), 0).unwrap();
        assert_eq!((stats.count, stats.max.as_deref()), (1000, Some("999")));
        let mut rows = 0;
        for_each_batch(&rs, &desc(Some(1000)), 300, &mut |b| {
            rows += b.num_rows();
            Ok(())
        })
        .unwrap();
        assert_eq!(rows, 1000, "export of a sorted view: its rows");

        // More rows loaded: a larger window; none given: every row that fits.
        assert_eq!(with_view(&rs, &desc(Some(5000)), |g| g.page(&desc(Some(5000)), 0, 1)).map(|p| first(&p)).unwrap(), vec!["4999"]);
        let all = with_view(&rs, &desc(None), |g| g.page(&desc(None), 0, 1)).unwrap();
        assert_eq!((first(&all), all.window_rows), (vec![(n - 1).to_string()], Some(n as usize)));

        // The plain result and find in it: every row, from disk.
        let plain = with_view(&rs, &ViewSpec::default(), |g| g.page(&ViewSpec::default(), 99_999, 1)).unwrap();
        assert_eq!((first(&plain), plain.window_rows), (vec!["99999".to_string()], None));
        // Statistics: of the rows in memory (the window), never the files.
        let loaded = ViewSpec { first_rows: Some(500), ..Default::default() };
        let s = column_stats(&rs, &loaded, 0).unwrap();
        assert_eq!((s.count, s.distinct, s.max.as_deref()), (500, 500, Some("499")));
        assert_eq!(find(&rs, &ViewSpec::default(), "99999", None, 10, false).unwrap().matches.len(), 1);
        // A chart: of the window too, even of the plain result.
        let count = ChartSpec { x: 0, y: vec![], agg: Agg::Count, series: None, limit: 5 };
        let plain = ViewSpec { first_rows: Some(200), ..Default::default() };
        let d = with_window(&rs, &plain, |g| g.chart(&plain, &count)).unwrap();
        assert_eq!((d.rows, d.window_rows), (200, Some(200)));
        let d = with_window(&rs, &desc(None), |g| g.chart(&desc(None), &count)).unwrap();
        assert_eq!((d.rows, d.window_rows), (n as usize, Some(n as usize)));
        // The window (the last one read: every row) counts as the result's memory.
        assert!(rs.lock().memory_bytes() >= n as usize * 8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::arrow::array::{Float64Array, Int64Array, StringArray};
    use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
    use databrain_connector_core::value::META_DB_TYPE;

    fn sample() -> ResultSet {
        let mut md = HashMap::new();
        md.insert(META_DB_TYPE.to_string(), "numeric".to_string());
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
            Field::new("score", DataType::Float64, true),
            Field::new("amount", DataType::Utf8, true).with_metadata(md),
        ]));
        let mut rs = ResultSet::new("r1", schema.clone());
        let b1 = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec![Some("Alice"), Some("bob"), None])),
                Arc::new(Float64Array::from(vec![Some(9.5), None, Some(7.0)])),
                Arc::new(StringArray::from(vec!["10.5", "9", "100"])),
            ],
        )
        .unwrap();
        let b2 = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![4, 5])),
                Arc::new(StringArray::from(vec![Some("alicia"), Some("Carol")])),
                Arc::new(Float64Array::from(vec![Some(8.0), Some(9.5)])),
                Arc::new(StringArray::from(vec!["2", "10.25"])),
            ],
        )
        .unwrap();
        rs.push(b1);
        rs.push(b2);
        rs.finish(false);
        rs
    }

    fn ids(p: &Page) -> Vec<String> {
        p.rows.iter().map(|r| r[0].clone().unwrap()).collect()
    }

    #[test]
    fn identity_page_spans_batches() {
        let mut rs = sample();
        let p = rs.page(&ViewSpec::default(), 2, 2).unwrap();
        assert_eq!(ids(&p), vec!["3", "4"]);
        assert_eq!(p.row_ids, vec![2, 3]);
        assert_eq!(p.rows[0][1], None);
        assert_eq!(p.view_rows, 5);
    }

    #[test]
    fn filter_contains_and_numeric() {
        let mut rs = sample();
        let spec = ViewSpec {
            filters: vec![
                ColumnFilter { column: 1, op: FilterOp::Contains, value: "ALI".into() },
                ColumnFilter { column: 2, op: FilterOp::Gte, value: "8".into() },
            ],
            ..Default::default()
        };
        let p = rs.page(&spec, 0, 100).unwrap();
        assert_eq!(ids(&p), vec!["1", "4"]);
        assert_eq!(p.view_rows, 2);

        let bad = ViewSpec { filters: vec![ColumnFilter { column: 0, op: FilterOp::Gt, value: "abc".into() }], ..Default::default() };
        assert!(matches!(rs.page(&bad, 0, 10), Err(Error::InvalidFilterValue(_))));
    }

    #[test]
    fn selection_summary() {
        let mut rs = sample();
        let s = rs.summarize(&ViewSpec::default(), &[2, 3], 0, 100).unwrap();
        assert!(s.cells > 0 && s.numbers > 0 && s.sum.is_some() && s.min <= s.max, "{s:?}");
        let names = rs.summarize(&ViewSpec::default(), &[1], 0, 100).unwrap();
        assert_eq!((names.numbers, names.sum), (0, None), "text is counted, not summed");
        assert_eq!(names.cells, rs.info().total_rows);
    }

    #[test]
    fn whole_word_find() {
        assert!(contains_word("a id", "id") && contains_word("id", "id") && contains_word("user-id", "id"));
        assert!(!contains_word("paid", "id") && !contains_word("id_x", "id") && !contains_word("idx", "id"));
        assert!(contains_word("x paid id", "id"), "a later occurrence counts");
        assert!(contains_word("é id", "id") && !contains_word("éid", "id"));
        assert!(contains_word("a.b", ".b"), "punctuation at the edge needs no boundary");
        let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, true)]));
        let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec![Some("paid"), Some("ID 7"), None, Some("my id")]))]).unwrap();
        let mut rs = ResultSet::new("w", schema);
        rs.push(b);
        let rows = |rs: &mut ResultSet, w: bool| -> Vec<u32> {
            rs.find_with(&ViewSpec::default(), "id", None, 10, w).unwrap().matches.iter().map(|m| m.row).collect()
        };
        assert_eq!(rows(&mut rs, false), vec![0, 1, 3]);
        assert_eq!(rows(&mut rs, true), vec![1, 3]);
    }

    #[test]
    fn sort_multi_and_decimal_text() {
        let mut rs = sample();
        let spec = ViewSpec { sort: vec![SortKey { column: 2, descending: true }, SortKey { column: 0, descending: false }], ..Default::default() };
        // nulls last
        assert_eq!(ids(&rs.page(&spec, 0, 10).unwrap()), vec!["1", "5", "4", "3", "2"]);
        let spec = ViewSpec { sort: vec![SortKey { column: 3, descending: false }], ..Default::default() };
        // numeric order, not lexicographic
        assert_eq!(ids(&rs.page(&spec, 0, 10).unwrap()), vec!["4", "2", "5", "1", "3"]);
    }

    #[test]
    fn quick_filter_and_find() {
        let mut rs = sample();
        let spec = ViewSpec { quick_filter: Some("9.5".into()), ..Default::default() };
        assert_eq!(ids(&rs.page(&spec, 0, 10).unwrap()), vec!["1", "5"]);
        let f = rs.find(&ViewSpec::default(), "ali", 100).unwrap();
        assert_eq!(f.matches.len(), 2);
        assert_eq!((f.matches[1].row, f.matches[1].col), (3, 1));
        let f = rs.find(&ViewSpec::default(), "1", 2).unwrap();
        assert!(f.truncated);
        // LIKE metacharacters are literal
        assert!(rs.find(&ViewSpec::default(), "%", 10).unwrap().matches.is_empty());
        // Only in chosen columns: "1" appears in ids and scores, column 0 only here.
        let all = rs.find(&ViewSpec::default(), "1", 100).unwrap();
        let ids_only = rs.find_in(&ViewSpec::default(), "1", Some(&[0]), 100).unwrap();
        assert!(ids_only.matches.iter().all(|m| m.col == 0));
        assert!(!ids_only.matches.is_empty() && ids_only.matches.len() <= all.matches.len());
        assert!(rs.find_in(&ViewSpec::default(), "ali", Some(&[0]), 100).unwrap().matches.is_empty());
        assert_eq!(rs.find_in(&ViewSpec::default(), "ali", Some(&[1, 99]), 100).unwrap().matches.len(), 2);
    }

    #[test]
    fn stats() {
        let mut rs = sample();
        let s = rs.column_stats(&ViewSpec::default(), 2).unwrap();
        assert_eq!((s.count, s.nulls, s.distinct), (5, 1, 3));
        assert_eq!(s.min.as_deref(), Some("7.0"));
        assert_eq!(s.max.as_deref(), Some("9.5"));
        assert_eq!(s.top[0].value.as_deref(), Some("9.5"));
        let s = rs.column_stats(&ViewSpec::default(), 3).unwrap();
        assert_eq!(s.max.as_deref(), Some("100"));
    }

    #[test]
    fn view_batches_and_store() {
        let store = ResultStore::new();
        let rs = sample();
        let schema = rs.schema();
        let h = store.create("x", schema);
        *h.lock() = rs;
        let got = store.get("x").unwrap();
        let bs = got.lock().view_batches(&ViewSpec::default(), 2).unwrap();
        assert_eq!(bs.iter().map(|b| b.num_rows()).collect::<Vec<_>>(), vec![2, 2, 1]);
        assert!(store.remove("x"));
        assert!(store.get("x").is_err());
    }

    /// Same operations on a result whose text passes the 32-bit limit
    /// (lowered here so the test stays small): combined with 64-bit
    /// offsets, handed out with the result's own types.
    #[test]
    fn text_over_the_offset_limit() {
        let mut rs = sample();
        rs.wide_limit = 8; // "Alice" + "bob" + "alicia" + "Carol" = 19 bytes
        let c = rs.combined().unwrap();
        assert_eq!(c.schema().field(1).data_type(), &DataType::LargeUtf8);
        assert_eq!(c.schema().field(3).data_type(), &DataType::LargeUtf8, "decimal text too (14 bytes)");
        assert!(display::is_decimal_text(c.schema().field(3)), "metadata kept");
        assert_eq!(c.schema().field(0).data_type(), &DataType::Int64);
        // Paging, filters, quick filter, sort (decimal text numeric), find, stats.
        assert_eq!(ids(&rs.page(&ViewSpec::default(), 3, 10).unwrap()), vec!["4", "5"]);
        let f = ViewSpec { filters: vec![ColumnFilter { column: 1, op: FilterOp::Contains, value: "ALI".into() }], ..Default::default() };
        assert_eq!(ids(&rs.page(&f, 0, 10).unwrap()), vec!["1", "4"]);
        let eq = ViewSpec { filters: vec![ColumnFilter { column: 1, op: FilterOp::Equals, value: "bob".into() }], ..Default::default() };
        assert_eq!(ids(&rs.page(&eq, 0, 10).unwrap()), vec!["2"]);
        let q = ViewSpec { quick_filter: Some("carol".into()), ..Default::default() };
        assert_eq!(ids(&rs.page(&q, 0, 10).unwrap()), vec!["5"]);
        let sorted = ViewSpec { sort: vec![SortKey { column: 3, descending: false }], ..Default::default() };
        assert_eq!(ids(&rs.page(&sorted, 0, 10).unwrap()), vec!["4", "2", "5", "1", "3"]);
        let by_name = ViewSpec { sort: vec![SortKey { column: 1, descending: false }], ..Default::default() };
        assert_eq!(ids(&rs.page(&by_name, 0, 10).unwrap()), vec!["1", "5", "4", "2", "3"]);
        assert_eq!(rs.find(&ViewSpec::default(), "ali", 10).unwrap().matches.len(), 2);
        assert_eq!(rs.column_stats(&ViewSpec::default(), 1).unwrap().distinct, 4);
        // Slices and export/output chunks have the result's own types.
        let (slice, _) = rs.view_slice(&sorted, 0, 2).unwrap();
        assert_eq!(slice.schema().field(1).data_type(), &DataType::Utf8);
        for b in rs.view_batches(&f, 1).unwrap() {
            assert_eq!(b.schema(), rs.schema());
        }
    }

    #[test]
    fn display_text_switches_to_64_bit_offsets() {
        let a = Int64Array::from(vec![Some(12345), None, Some(678)]);
        let small = display::to_display_limited(&a, 100).unwrap();
        assert!(matches!(small, display::DisplayText::Small(_)));
        let large = display::to_display_limited(&a, 6).unwrap();
        assert!(matches!(large, display::DisplayText::Large(_)));
        assert_eq!((large.value(0), large.is_null(1), large.value(2), large.len()), ("12345", true, "678", 3));
        let m = large.ilike("%7%", false).unwrap();
        assert_eq!((m.value(0), m.is_null(1), m.value(2)), (false, true, true));
    }

    /// Real 2 GiB+: `cargo test -p databrain-result-store --release -- --ignored over_2gib` (about 7 GB of RAM).
    #[test]
    #[ignore]
    fn over_2gib_of_text() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false), Field::new("doc", DataType::Utf8, false)]));
        let mut rs = ResultSet::new("big", schema.clone());
        let doc = "x".repeat(1 << 20); // 1 MiB per value
        let per_batch = 256; // 256 MiB per batch
        for b in 0..9 {
            let ids: Vec<i64> = (0..per_batch).map(|i| (b * per_batch + i) as i64).collect();
            let docs = StringArray::from(vec![doc.as_str(); per_batch]);
            rs.push(RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(ids)), Arc::new(docs)]).unwrap());
        }
        rs.finish(false);
        // 2.25 GiB of text in one column: the case that failed with "Offset overflow".
        let page = rs.page(&ViewSpec::default(), 2300, 2).unwrap();
        assert_eq!(page.row_ids, vec![2300, 2301]);
        assert_eq!(rs.view_slice(&ViewSpec::default(), 2300, 2).unwrap().0.schema(), schema, "pages get the result's own types");
        let sorted = ViewSpec { sort: vec![SortKey { column: 0, descending: true }], ..Default::default() };
        assert_eq!(rs.page(&sorted, 0, 1).unwrap().row_ids, vec![2303]);
        // One chunk asked for everything: split into chunks that fit 32-bit offsets.
        let chunks = rs.view_batches(&ViewSpec::default(), 1_000_000).unwrap();
        assert!(chunks.len() >= 2, "{}", chunks.len());
        assert_eq!(chunks.iter().map(|b| b.num_rows()).sum::<usize>(), 2304);
        assert!(chunks.iter().all(|b| b.schema() == schema));
    }

    /// One column of Oracle-style `NUMBER(38)` text plus a row id.
    fn decimals(db_type: &str, vals: &[Option<&str>]) -> ResultSet {
        let mut md = HashMap::new();
        md.insert(META_DB_TYPE.to_string(), db_type.to_string());
        let schema = Arc::new(Schema::new(vec![Field::new("row", DataType::Int64, false), Field::new("n", DataType::Utf8, true).with_metadata(md)]));
        let mut rs = ResultSet::new("d", schema.clone());
        let rows = Int64Array::from((0..vals.len() as i64).collect::<Vec<_>>());
        rs.push(RecordBatch::try_new(schema, vec![Arc::new(rows), Arc::new(StringArray::from(vals.to_vec()))]).unwrap());
        rs.finish(false);
        rs
    }

    fn view_rows(rs: &mut ResultSet, spec: &ViewSpec) -> Vec<u32> {
        rs.page(spec, 0, 100).unwrap().row_ids
    }

    fn filter(op: FilterOp, v: &str) -> ViewSpec {
        ViewSpec { filters: vec![ColumnFilter { column: 1, op, value: v.into() }], ..Default::default() }
    }

    /// Ids past 2^53 differ in the last digit only; as f64 they were equal.
    #[test]
    fn large_decimals_compare_exactly() {
        let big = ["9007199254740993", "9007199254740992", "9007199254740994", "-9007199254740993", "-9007199254740992"];
        let mut rs = decimals("NUMBER(38)", &big.map(Some));
        let asc = ViewSpec { sort: vec![SortKey { column: 1, descending: false }], ..Default::default() };
        assert_eq!(view_rows(&mut rs, &asc), vec![3, 4, 1, 0, 2]);
        let desc = ViewSpec { sort: vec![SortKey { column: 1, descending: true }], ..Default::default() };
        assert_eq!(view_rows(&mut rs, &desc), vec![2, 0, 1, 4, 3]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Equals, "9007199254740993")), vec![0]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::NotEquals, "9007199254740993")), vec![1, 2, 3, 4]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Gt, "9007199254740992")), vec![0, 2]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Gte, "9007199254740993")), vec![0, 2]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Lt, "-9007199254740992")), vec![3]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Lte, "-9007199254740992")), vec![3, 4]);
        // 38 digits: beyond i64 and f64.
        let mut rs = decimals("NUMBER(38)", &[Some("99999999999999999999999999999999999999"), Some("99999999999999999999999999999999999998")]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Equals, "99999999999999999999999999999999999998")), vec![1]);
        assert_eq!(view_rows(&mut rs, &asc), vec![1, 0]);

        let mut rs = decimals("NUMBER(38)", &big.map(Some));
        let stats = rs.column_stats(&ViewSpec::default(), 1).unwrap();
        assert_eq!(stats.distinct, 5);
        assert_eq!((stats.min.as_deref(), stats.max.as_deref()), (Some("-9007199254740993"), Some("9007199254740994")));
        let s = rs.summarize(&ViewSpec::default(), &[1], 0, 100).unwrap();
        assert_eq!((s.numbers, s.distinct), (5, 5));
        // Exact sum: 9007199254740994 (one rounding at the end, to the f64 field).
        assert_eq!(s.sum, Some(9007199254740994.0));
        assert_eq!((s.min, s.max), (Some(-9007199254740993.0), Some(9007199254740994.0)));
        // Exact sum where adding as f64 drifts: 2^53 + 1 + 1 = 2^53 + 2.
        let mut rs = decimals("numeric", &[Some("9007199254740992"), Some("1"), Some("1")]);
        assert_eq!(rs.summarize(&ViewSpec::default(), &[1], 0, 100).unwrap().sum, Some(9007199254740994.0));
        // 0.1 + 0.2 is 0.3 exactly.
        let mut rs = decimals("numeric(3,1)", &[Some("0.1"), Some("0.2")]);
        assert_eq!(rs.summarize(&ViewSpec::default(), &[1], 0, 100).unwrap().sum, Some(0.3));
    }

    /// Ordinary decimals: scale differences, negatives, zero forms and NULLs.
    #[test]
    fn decimal_text_scale_sign_and_nulls() {
        let vals = [Some("1.50"), Some("-2.5"), None, Some("1.5"), Some("-0.000"), Some("10"), Some("0.05"), Some("-10.25"), Some("1.500001")];
        let mut rs = decimals("numeric(12,6)", &vals);
        let asc = ViewSpec { sort: vec![SortKey { column: 1, descending: false }, SortKey { column: 0, descending: false }], ..Default::default() };
        assert_eq!(view_rows(&mut rs, &asc), vec![7, 1, 4, 6, 0, 3, 8, 5, 2], "nulls last");
        let desc = ViewSpec { sort: vec![SortKey { column: 1, descending: true }, SortKey { column: 0, descending: false }], ..Default::default() };
        assert_eq!(view_rows(&mut rs, &desc), vec![5, 8, 0, 3, 6, 4, 1, 7, 2], "nulls last");
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Equals, "1.5")), vec![0, 3]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Equals, " 1.5000 ")), vec![0, 3]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Equals, "0")), vec![4], "-0.000 is zero");
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::NotEquals, "1.5")), vec![1, 4, 5, 6, 7, 8], "NULL is neither equal nor unequal");
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Lt, "0")), vec![1, 7]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Gt, "1.5")), vec![5, 8]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Gte, "-2.50")), vec![0, 1, 3, 4, 5, 6, 8]);
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::Equals, "1e1")), vec![5], "exponent form");
        assert_eq!(view_rows(&mut rs, &filter(FilterOp::IsNull, "")), vec![2]);
        assert!(matches!(rs.page(&filter(FilterOp::Gt, "abc"), 0, 10), Err(Error::InvalidFilterValue(_))));

        let stats = rs.column_stats(&ViewSpec::default(), 1).unwrap();
        assert_eq!((stats.count, stats.nulls, stats.distinct), (9, 1, 7), "1.50 and 1.5 are one value");
        assert_eq!((stats.min.as_deref(), stats.max.as_deref()), (Some("-10.25"), Some("10")));
        assert_eq!((stats.top[0].value.as_deref(), stats.top[0].count), (Some("1.50"), 2));

        let s = rs.summarize(&ViewSpec::default(), &[1], 0, 100).unwrap();
        assert_eq!((s.cells, s.non_null, s.numbers, s.distinct), (9, 8, 8, 7));
        assert_eq!(s.sum, Some(1.800001));
        assert_eq!((s.min, s.max), (Some(-10.25), Some(10.0)));
        assert_eq!(s.avg, Some(1.800001 / 8.0));
        let none = decimals("numeric", &[None, None]).summarize(&ViewSpec::default(), &[1], 0, 100).unwrap();
        assert_eq!((none.numbers, none.sum, none.min, none.distinct), (0, None, None, 0));
    }

    /// Floats keep their f64 behaviour, also next to decimals in one selection.
    #[test]
    fn floats_unchanged_next_to_decimals() {
        let mut rs = sample();
        let s = rs.summarize(&ViewSpec::default(), &[2, 3], 0, 100).unwrap();
        assert_eq!(s.numbers, 9);
        assert_eq!(s.sum, Some(9.5 + 7.0 + 8.0 + 9.5 + 10.5 + 9.0 + 100.0 + 2.0 + 10.25));
        assert_eq!((s.min, s.max), (Some(2.0), Some(100.0)));
        let spec = ViewSpec { filters: vec![ColumnFilter { column: 2, op: FilterOp::Equals, value: "9.5".into() }], ..Default::default() };
        assert_eq!(ids(&rs.page(&spec, 0, 10).unwrap()), vec!["1", "5"]);
        let spec = ViewSpec { filters: vec![ColumnFilter { column: 3, op: FilterOp::Equals, value: "10.50".into() }], ..Default::default() };
        assert_eq!(ids(&rs.page(&spec, 0, 10).unwrap()), vec!["1"]);
    }

    #[test]
    fn view_spec_json() {
        let s: ViewSpec = serde_json::from_str(r#"{"filters":[{"column":1,"op":"is_null"}],"sort":[{"column":0}]}"#).unwrap();
        assert_eq!(s.filters[0].op, FilterOp::IsNull);
        assert!(!s.sort[0].descending);
    }
}
