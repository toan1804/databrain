//! In-memory store for query results with local view operations
//! (filter, quick filter, sort, find, column stats) and paging for the grid.

pub mod chart;
pub mod display;
pub mod view;
mod wide;

use std::collections::HashMap;
use std::sync::Arc;

use databrain_connector_core::arrow::array::{Array, RecordBatch, UInt32Array};
use databrain_connector_core::arrow::compute;
use databrain_connector_core::arrow::datatypes::SchemaRef;
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
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A page of display-ready cells for the grid.
#[derive(Debug, Clone, Serialize)]
pub struct Page {
    /// Rows in the current view (after filters).
    pub view_rows: usize,
    /// Rows stored in the result (before filters).
    pub total_rows: usize,
    pub offset: usize,
    /// 0-based index of each row in the original result.
    pub row_ids: Vec<u32>,
    /// `rows[r][c]`; `None` is SQL NULL.
    pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CellMatch {
    /// Row position within the view.
    pub row: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindResult {
    pub matches: Vec<CellMatch>,
    /// True when the match list was cut at the limit.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopValue {
    pub value: Option<String>,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnStats {
    pub column: usize,
    pub count: u64,
    pub nulls: u64,
    pub distinct: u64,
    pub min: Option<String>,
    pub max: Option<String>,
    pub top: Vec<TopValue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResultInfo {
    pub id: String,
    pub columns: Vec<ColumnMeta>,
    pub total_rows: usize,
    pub complete: bool,
    /// Stopped at the row limit; more rows exist on the server.
    pub truncated: bool,
    pub bytes: usize,
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
        }
    }

    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    pub fn total_rows(&self) -> usize {
        self.total_rows
    }

    pub fn push(&mut self, batch: RecordBatch) {
        if batch.num_rows() == 0 {
            return;
        }
        self.total_rows += batch.num_rows();
        self.batches.push(batch);
        self.combined = None;
    }

    pub fn finish(&mut self, truncated: bool) {
        self.complete = true;
        self.truncated = truncated;
    }

    pub fn memory_bytes(&self) -> usize {
        match &self.combined {
            Some(c) => c.get_array_memory_size(),
            None => self.batches.iter().map(|b| b.get_array_memory_size()).sum(),
        }
    }

    pub fn info(&self) -> ResultInfo {
        ResultInfo {
            id: self.id.clone(),
            columns: self.schema.fields().iter().map(|f| column_meta(f)).collect(),
            total_rows: self.total_rows,
            complete: self.complete,
            truncated: self.truncated,
            bytes: self.memory_bytes(),
        }
    }

    /// All rows as one batch (concatenated lazily and cached). Text/binary
    /// columns with more than 2 GiB of values come back with 64-bit offsets
    /// (`LargeUtf8`/`LargeBinary`); use [`ResultSet::view_batches`] for
    /// batches with the result's own types.
    pub fn combined(&mut self) -> Result<RecordBatch> {
        if let Some(c) = &self.combined {
            return Ok(c.clone());
        }
        let c = if self.batches.len() == 1 {
            self.batches[0].clone()
        } else {
            wide::concat_wide(&self.schema, &self.batches, self.wide_limit)?
        };
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
        if let Some(c) = &self.view_cache {
            if &c.spec == spec && c.rows == self.total_rows {
                return Ok(Some(c.indices.clone()));
            }
        }
        let batch = self.combined()?;
        let idx = Arc::new(view::compute_view(&batch, spec, &mut self.display_cache)?);
        self.view_cache = Some(ViewCache {
            spec: spec.clone(),
            rows: self.total_rows,
            indices: idx.clone(),
        });
        Ok(Some(idx))
    }

    pub fn view_len(&mut self, spec: &ViewSpec) -> Result<usize> {
        Ok(match self.view_indices(spec)? {
            Some(i) => i.len(),
            None => self.total_rows,
        })
    }

    /// Rows `[offset, offset+limit)` of the view as a batch plus original ids.
    pub fn view_slice(
        &mut self,
        spec: &ViewSpec,
        offset: usize,
        limit: usize,
    ) -> Result<(RecordBatch, Vec<u32>)> {
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
        let view_rows = self.view_len(spec)?;
        let (batch, row_ids) = self.view_slice(spec, offset, limit)?;
        let opts = display::format_options();
        let formatters: Vec<ArrayFormatter<'_>> = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &opts))
            .collect::<Result<_, _>>()?;
        let rows = (0..batch.num_rows())
            .map(|r| {
                formatters
                    .iter()
                    .zip(batch.columns())
                    .map(|(f, a)| display::cell(f, a, r))
                    .collect()
            })
            .collect();
        Ok(Page {
            view_rows,
            total_rows: self.total_rows,
            offset,
            row_ids,
            rows,
        })
    }

    /// Find cells containing `query` (case-insensitive) within the view.
    pub fn find(&mut self, spec: &ViewSpec, query: &str, limit: usize) -> Result<FindResult> {
        self.find_in(spec, query, None, limit)
    }

    /// Find `query` (case-insensitive substring of the displayed value),
    /// optionally only in the given columns.
    pub fn find_in(&mut self, spec: &ViewSpec, query: &str, columns: Option<&[usize]>, limit: usize) -> Result<FindResult> {
        let mut matches = Vec::new();
        if query.is_empty() || self.total_rows == 0 {
            return Ok(FindResult {
                matches,
                truncated: false,
            });
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
                masks.push(Some(text.ilike(&pattern, false)?));
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
                        return Ok(FindResult {
                            matches,
                            truncated: true,
                        });
                    }
                    matches.push(CellMatch {
                        row: vr as u32,
                        col: c as u32,
                    });
                }
            }
        }
        Ok(FindResult {
            matches,
            truncated: false,
        })
    }

    /// Statistics of one column over the current view.
    pub fn column_stats(&mut self, spec: &ViewSpec, column: usize) -> Result<ColumnStats> {
        if column >= self.schema.fields().len() {
            return Err(Error::InvalidColumn(column));
        }
        let n = self.view_len(spec)?;
        let (batch, _) = self.view_slice(spec, 0, n)?;
        let col = batch.column(column).clone();
        let field = self.schema.field(column).clone();
        let nulls = col.null_count() as u64;

        // min / max via sort (works for every orderable type).
        let sortable = if display::is_decimal_text(&field) {
            compute::cast(&col, &databrain_connector_core::arrow::datatypes::DataType::Float64)?
        } else {
            col.clone()
        };
        let (mut min, mut max) = (None, None);
        if col.len() > nulls as usize {
            if let Ok(order) = compute::sort_to_indices(
                sortable.as_ref(),
                Some(compute::SortOptions {
                    descending: false,
                    nulls_first: false,
                }),
                None,
            ) {
                let text = display::to_display(col.as_ref())?;
                let non_null: Vec<u32> = order
                    .values()
                    .iter()
                    .copied()
                    .filter(|i| !sortable.is_null(*i as usize))
                    .collect();
                if let (Some(first), Some(last)) = (non_null.first(), non_null.last()) {
                    min = Some(text.value(*first as usize).to_string());
                    max = Some(text.value(*last as usize).to_string());
                }
            }
        }

        let text = display::to_display(col.as_ref())?;
        let mut counts: HashMap<Option<&str>, u64> = HashMap::new();
        for i in 0..text.len() {
            let k = if text.is_null(i) {
                None
            } else {
                Some(text.value(i))
            };
            *counts.entry(k).or_default() += 1;
        }
        let distinct = counts.keys().filter(|k| k.is_some()).count() as u64;
        let mut top: Vec<(Option<&str>, u64)> = counts.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let top = top
            .into_iter()
            .take(10)
            .map(|(v, count)| TopValue {
                value: v.map(|s| s.to_string()),
                count,
            })
            .collect();

        Ok(ColumnStats {
            column,
            count: col.len() as u64,
            nulls,
            distinct,
            min,
            max,
            top,
        })
    }

    /// The view as a sequence of batches (for export, outputs), at most
    /// `chunk` rows each, always with the result's own column types: a chunk
    /// whose text passes 2 GiB is split further.
    pub fn view_batches(&mut self, spec: &ViewSpec, chunk: usize) -> Result<Vec<RecordBatch>> {
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
}

impl ResultStore {
    pub fn new() -> Self {
        Self::default()
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

    pub fn bytes_of(&self, id: &str) -> usize {
        self.results.lock().get(id).map(|r| r.lock().memory_bytes()).unwrap_or(0)
    }

    pub fn create(&self, id: impl Into<String>, schema: SchemaRef) -> Arc<Mutex<ResultSet>> {
        let id = id.into();
        let rs = Arc::new(Mutex::new(ResultSet::new(id.clone(), schema)));
        self.results.lock().insert(id, rs.clone());
        rs
    }

    pub fn get(&self, id: &str) -> Result<Arc<Mutex<ResultSet>>> {
        self.results
            .lock()
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    pub fn remove(&self, id: &str) -> bool {
        self.results.lock().remove(id).is_some()
    }

    pub fn total_bytes(&self) -> usize {
        self.results
            .lock()
            .values()
            .map(|r| r.lock().memory_bytes())
            .sum()
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
                ColumnFilter {
                    column: 1,
                    op: FilterOp::Contains,
                    value: "ALI".into(),
                },
                ColumnFilter {
                    column: 2,
                    op: FilterOp::Gte,
                    value: "8".into(),
                },
            ],
            ..Default::default()
        };
        let p = rs.page(&spec, 0, 100).unwrap();
        assert_eq!(ids(&p), vec!["1", "4"]);
        assert_eq!(p.view_rows, 2);

        let bad = ViewSpec {
            filters: vec![ColumnFilter {
                column: 0,
                op: FilterOp::Gt,
                value: "abc".into(),
            }],
            ..Default::default()
        };
        assert!(matches!(rs.page(&bad, 0, 10), Err(Error::InvalidFilterValue(_))));
    }

    #[test]
    fn sort_multi_and_decimal_text() {
        let mut rs = sample();
        let spec = ViewSpec {
            sort: vec![
                SortKey {
                    column: 2,
                    descending: true,
                },
                SortKey {
                    column: 0,
                    descending: false,
                },
            ],
            ..Default::default()
        };
        // nulls last
        assert_eq!(ids(&rs.page(&spec, 0, 10).unwrap()), vec!["1", "5", "4", "3", "2"]);
        let spec = ViewSpec {
            sort: vec![SortKey {
                column: 3,
                descending: false,
            }],
            ..Default::default()
        };
        // numeric order, not lexicographic
        assert_eq!(ids(&rs.page(&spec, 0, 10).unwrap()), vec!["4", "2", "5", "1", "3"]);
    }

    #[test]
    fn quick_filter_and_find() {
        let mut rs = sample();
        let spec = ViewSpec {
            quick_filter: Some("9.5".into()),
            ..Default::default()
        };
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

    #[test]
    fn view_spec_json() {
        let s: ViewSpec = serde_json::from_str(
            r#"{"filters":[{"column":1,"op":"is_null"}],"sort":[{"column":0}]}"#,
        )
        .unwrap();
        assert_eq!(s.filters[0].op, FilterOp::IsNull);
        assert!(!s.sort[0].descending);
    }
}
