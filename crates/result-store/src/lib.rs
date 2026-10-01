//! In-memory store for query results with local view operations
//! (filter, quick filter, sort, find, column stats) and paging for the grid.

pub mod chart;
pub mod display;
pub mod view;

use std::collections::HashMap;
use std::sync::Arc;

use databrain_connector_core::arrow::array::{Array, RecordBatch, UInt32Array};
use databrain_connector_core::arrow::compute::{self, concat_batches};
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

    /// All rows as one batch (concatenated lazily and cached).
    pub fn combined(&mut self) -> Result<RecordBatch> {
        if let Some(c) = &self.combined {
            return Ok(c.clone());
        }
        let c = if self.batches.len() == 1 {
            self.batches[0].clone()
        } else {
            concat_batches(&self.schema, &self.batches)?
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
        match self.view_indices(spec)? {
            None => {
                let start = offset.min(batch.num_rows());
                let len = limit.min(batch.num_rows() - start);
                let ids = (start as u32..(start + len) as u32).collect();
                Ok((batch.slice(start, len), ids))
            }
            Some(idx) => {
                let start = offset.min(idx.len());
                let len = limit.min(idx.len() - start);
                let part = idx.slice(start, len);
                let taken = compute::take_record_batch(&batch, &part)?;
                Ok((taken, part.values().to_vec()))
            }
        }
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
        let mut matches = Vec::new();
        if query.is_empty() || self.total_rows == 0 {
            return Ok(FindResult {
                matches,
                truncated: false,
            });
        }
        let batch = self.combined()?;
        let idx = self.view_indices(spec)?;
        let pat = view::like_escape(query);
        let pattern = databrain_connector_core::arrow::array::Scalar::new(
            databrain_connector_core::arrow::array::StringArray::from(vec![format!("%{pat}%")]),
        );
        let mut masks = Vec::with_capacity(batch.num_columns());
        for c in 0..batch.num_columns() {
            let text = self.display_cache.get(&batch, c)?;
            masks.push(compute::kernels::comparison::ilike(text.as_ref(), &pattern)?);
        }
        let hit = |row: usize, col: usize| masks[col].is_valid(row) && masks[col].value(row);
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

    /// The view as a sequence of batches (for export), `chunk` rows each.
    pub fn view_batches(&mut self, spec: &ViewSpec, chunk: usize) -> Result<Vec<RecordBatch>> {
        let n = self.view_len(spec)?;
        let chunk = chunk.max(1);
        let mut out = Vec::with_capacity(n.div_ceil(chunk));
        let mut off = 0;
        while off < n {
            out.push(self.view_slice(spec, off, chunk)?.0);
            off += chunk;
        }
        Ok(out)
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
