//! View computation: column filters, quick filter and multi-column sort over
//! a result, producing row indices into the underlying data.

use std::sync::Arc;

use databrain_connector_core::arrow::array::{
    Array, ArrayRef, BooleanArray, BooleanBuilder, RecordBatch, Scalar, StringArray, UInt32Array,
};
use databrain_connector_core::arrow::compute::kernels::cmp;
use databrain_connector_core::arrow::compute::kernels::comparison::{ilike, nilike};
use databrain_connector_core::arrow::compute::{
    self, CastOptions, SortColumn, SortOptions, and_kleene, cast_with_options, is_not_null,
    is_null, lexsort_to_indices, or_kleene,
};
use databrain_connector_core::arrow::datatypes::DataType;
use serde::{Deserialize, Serialize};

use crate::display::{self, is_decimal_text};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Contains,
    NotContains,
    Equals,
    NotEquals,
    StartsWith,
    EndsWith,
    Gt,
    Gte,
    Lt,
    Lte,
    IsNull,
    IsNotNull,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColumnFilter {
    pub column: usize,
    pub op: FilterOp,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SortKey {
    pub column: usize,
    #[serde(default)]
    pub descending: bool,
}

/// What the grid is showing. All parts are optional; the default is the raw
/// result in arrival order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ViewSpec {
    #[serde(default)]
    pub filters: Vec<ColumnFilter>,
    /// Keep rows where any cell contains this text (case-insensitive).
    #[serde(default)]
    pub quick_filter: Option<String>,
    #[serde(default)]
    pub sort: Vec<SortKey>,
}

impl ViewSpec {
    pub fn is_identity(&self) -> bool {
        self.filters.is_empty()
            && self.sort.is_empty()
            && self.quick_filter.as_deref().is_none_or(|q| q.is_empty())
    }
}

/// Escape `%`, `_` and `\` for LIKE patterns.
pub(crate) fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn pattern_scalar(p: String) -> Scalar<StringArray> {
    Scalar::new(StringArray::from(vec![p]))
}

/// Column as it should be compared: decimals-as-text become Float64.
fn comparable(batch: &RecordBatch, col: usize) -> Result<ArrayRef> {
    let field = batch.schema().field(col).clone();
    let a = batch.column(col).clone();
    if is_decimal_text(&field) {
        Ok(cast_with_options(&a, &DataType::Float64, &CastOptions::default())?)
    } else {
        Ok(a)
    }
}

fn display_col(cache: &mut DisplayCache, batch: &RecordBatch, col: usize) -> Result<Arc<StringArray>> {
    cache.get(batch, col)
}

/// Lazily computed display strings per column (for text matching).
#[derive(Default)]
pub struct DisplayCache {
    rows: usize,
    cols: Vec<Option<Arc<StringArray>>>,
}

impl DisplayCache {
    pub fn get(&mut self, batch: &RecordBatch, col: usize) -> Result<Arc<StringArray>> {
        if self.rows != batch.num_rows() || self.cols.len() != batch.num_columns() {
            self.rows = batch.num_rows();
            self.cols = vec![None; batch.num_columns()];
        }
        if let Some(c) = &self.cols[col] {
            return Ok(c.clone());
        }
        let s = Arc::new(display::to_display(batch.column(col).as_ref())?);
        self.cols[col] = Some(s.clone());
        Ok(s)
    }
}

fn filter_mask(
    batch: &RecordBatch,
    f: &ColumnFilter,
    cache: &mut DisplayCache,
) -> Result<BooleanArray> {
    if f.column >= batch.num_columns() {
        return Err(Error::InvalidColumn(f.column));
    }
    let raw = batch.column(f.column);
    match f.op {
        FilterOp::IsNull => return Ok(is_null(raw.as_ref())?),
        FilterOp::IsNotNull => return Ok(is_not_null(raw.as_ref())?),
        FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith => {
            let text = display_col(cache, batch, f.column)?;
            let v = like_escape(&f.value);
            let pat = match f.op {
                FilterOp::StartsWith => format!("{v}%"),
                FilterOp::EndsWith => format!("%{v}"),
                _ => format!("%{v}%"),
            };
            let s = pattern_scalar(pat);
            return Ok(if f.op == FilterOp::NotContains {
                nilike(text.as_ref(), &s)?
            } else {
                ilike(text.as_ref(), &s)?
            });
        }
        _ => {}
    }

    let col = comparable(batch, f.column)?;
    // Parse the user's value into the column's type.
    let lit = StringArray::from(vec![f.value.trim().to_string()]);
    let typed = if matches!(col.data_type(), DataType::Utf8) {
        Arc::new(StringArray::from(vec![f.value.clone()])) as ArrayRef
    } else {
        let opts = CastOptions {
            safe: false,
            ..Default::default()
        };
        cast_with_options(&lit, col.data_type(), &opts).map_err(|_| {
            Error::InvalidFilterValue(format!(
                "'{}' is not a valid {} value",
                f.value,
                display::family(batch.schema().field(f.column)).as_str()
            ))
        })?
    };
    let s = Scalar::new(typed);
    let a = col.as_ref();
    Ok(match f.op {
        FilterOp::Equals => cmp::eq(&a, &s)?,
        FilterOp::NotEquals => cmp::neq(&a, &s)?,
        FilterOp::Gt => cmp::gt(&a, &s)?,
        FilterOp::Gte => cmp::gt_eq(&a, &s)?,
        FilterOp::Lt => cmp::lt(&a, &s)?,
        FilterOp::Lte => cmp::lt_eq(&a, &s)?,
        _ => unreachable!("handled above"),
    })
}

/// Mask of rows where any column contains `q` (case-insensitive).
pub(crate) fn any_contains(
    batch: &RecordBatch,
    q: &str,
    cache: &mut DisplayCache,
) -> Result<BooleanArray> {
    let pat = pattern_scalar(format!("%{}%", like_escape(q)));
    let mut acc: Option<BooleanArray> = None;
    for c in 0..batch.num_columns() {
        let text = cache.get(batch, c)?;
        let m = ilike(text.as_ref(), &pat)?;
        acc = Some(match acc {
            None => m,
            Some(prev) => or_kleene(&prev, &m)?,
        });
    }
    Ok(acc.unwrap_or_else(|| {
        let mut b = BooleanBuilder::with_capacity(batch.num_rows());
        b.append_n(batch.num_rows(), false);
        b.finish()
    }))
}

fn true_indices(mask: &BooleanArray) -> UInt32Array {
    let mut out = Vec::with_capacity(mask.true_count());
    for i in 0..mask.len() {
        if mask.is_valid(i) && mask.value(i) {
            out.push(i as u32);
        }
    }
    UInt32Array::from(out)
}

/// Compute the row indices (into `batch`) that make up the view.
pub fn compute_view(
    batch: &RecordBatch,
    spec: &ViewSpec,
    cache: &mut DisplayCache,
) -> Result<UInt32Array> {
    let n = batch.num_rows();
    let mut mask: Option<BooleanArray> = None;
    for f in &spec.filters {
        let m = filter_mask(batch, f, cache)?;
        mask = Some(match mask {
            None => m,
            Some(prev) => and_kleene(&prev, &m)?,
        });
    }
    if let Some(q) = spec.quick_filter.as_deref().filter(|q| !q.is_empty()) {
        let m = any_contains(batch, q, cache)?;
        mask = Some(match mask {
            None => m,
            Some(prev) => and_kleene(&prev, &m)?,
        });
    }
    let idx = match &mask {
        Some(m) => true_indices(m),
        None => UInt32Array::from_iter_values(0..n as u32),
    };
    if spec.sort.is_empty() || idx.is_empty() {
        return Ok(idx);
    }

    let mut sort_cols = Vec::with_capacity(spec.sort.len());
    for k in &spec.sort {
        if k.column >= batch.num_columns() {
            return Err(Error::InvalidColumn(k.column));
        }
        let col = comparable(batch, k.column)?;
        let values = if mask.is_some() {
            compute::take(col.as_ref(), &idx, None)?
        } else {
            col
        };
        sort_cols.push(SortColumn {
            values,
            options: Some(SortOptions {
                descending: k.descending,
                nulls_first: false,
            }),
        });
    }
    let perm = lexsort_to_indices(&sort_cols, None)?;
    let sorted = compute::take(&idx, &perm, None)?;
    Ok(sorted
        .as_any()
        .downcast_ref::<UInt32Array>()
        .expect("take on UInt32Array returns UInt32Array")
        .clone())
}

impl display::TypeFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            display::TypeFamily::Number => "number",
            display::TypeFamily::Text => "text",
            display::TypeFamily::Bool => "boolean",
            display::TypeFamily::Date => "date/time",
            display::TypeFamily::Time => "time",
            display::TypeFamily::Binary => "binary",
            display::TypeFamily::Other => "value",
        }
    }
}
