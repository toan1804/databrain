//! Chart data: group a result view by an x column (optionally split into
//! series by another column) and aggregate y columns. Runs in Rust over the
//! stored Arrow data so charts work for any output without re-querying.

use std::collections::HashMap;

use databrain_connector_core::arrow::array::{Array, Float64Array};
use databrain_connector_core::arrow::compute::cast;
use databrain_connector_core::arrow::datatypes::DataType;
use serde::{Deserialize, Serialize};

use crate::display::{self, TypeFamily};
use crate::{Error, ResultSet, Result, ViewSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Agg {
    #[default]
    Sum,
    Avg,
    Count,
    Min,
    Max,
    /// Plot values as they are (one point per row; no grouping).
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartSpec {
    pub x: usize,
    /// Value columns. Empty with `Count` counts rows.
    #[serde(default)]
    pub y: Vec<usize>,
    #[serde(default)]
    pub agg: Agg,
    /// Split into one series per distinct value of this column (first y only).
    #[serde(default)]
    pub series: Option<usize>,
    /// Max categories on the x axis (largest totals kept for categorical x).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    500
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChartSeries {
    pub name: String,
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChartData {
    pub x: Vec<String>,
    /// `number`, `date`, `time` or `text` (the UI picks an axis type).
    pub x_family: String,
    pub series: Vec<ChartSeries>,
    /// Categories or series were cut to the limit.
    pub truncated: bool,
    pub rows: usize,
}

const MAX_SERIES: usize = 20;

#[derive(Clone, Copy, Default)]
struct Acc {
    sum: f64,
    n: u64,
    rows: u64,
    min: Option<f64>,
    max: Option<f64>,
}

impl Acc {
    fn add(&mut self, v: Option<f64>) {
        self.rows += 1;
        if let Some(v) = v {
            self.sum += v;
            self.n += 1;
            self.min = Some(self.min.map_or(v, |m| m.min(v)));
            self.max = Some(self.max.map_or(v, |m| m.max(v)));
        }
    }
    fn get(&self, agg: Agg) -> Option<f64> {
        match agg {
            Agg::Sum | Agg::None => (self.n > 0).then_some(self.sum),
            Agg::Avg => (self.n > 0).then(|| self.sum / self.n as f64),
            Agg::Count => Some(self.rows as f64),
            Agg::Min => self.min,
            Agg::Max => self.max,
        }
    }
}

fn family_str(f: TypeFamily) -> &'static str {
    match f {
        TypeFamily::Number => "number",
        TypeFamily::Date => "date",
        TypeFamily::Time => "time",
        _ => "text",
    }
}

impl ResultSet {
    pub fn chart(&mut self, view: &ViewSpec, spec: &ChartSpec) -> Result<ChartData> {
        let schema = self.schema();
        let ncols = schema.fields().len();
        for c in std::iter::once(spec.x).chain(spec.y.iter().copied()).chain(spec.series) {
            if c >= ncols {
                return Err(Error::InvalidColumn(c));
            }
        }
        let n = self.view_len(view)?;
        let (batch, _) = self.view_slice(view, 0, n)?;
        let xfam = display::family(schema.field(spec.x));
        let xs = display::to_display(batch.column(spec.x).as_ref())?;
        let nums: Vec<Float64Array> = spec
            .y
            .iter()
            .map(|&c| {
                let col = batch.column(c);
                let as_f = if display::is_decimal_text(schema.field(c)) || col.data_type().is_numeric() || matches!(col.data_type(), DataType::Boolean | DataType::Utf8 | DataType::LargeUtf8) {
                    cast(col, &DataType::Float64).or_else(|_| cast(&cast(col, &DataType::Utf8)?, &DataType::Float64))
                } else {
                    cast(col, &DataType::Float64)
                };
                as_f.map(|a| a.as_any().downcast_ref::<Float64Array>().cloned().unwrap_or_else(|| Float64Array::from(vec![None; batch.num_rows()])))
                    .map_err(Error::from)
            })
            .collect::<Result<_>>()?;
        let series_col = spec.series.map(|c| display::to_display(batch.column(c).as_ref())).transpose()?;
        let label = |a: &display::DisplayText, i: usize| if a.is_null(i) { "NULL".to_string() } else { a.value(i).to_string() };
        let ynames: Vec<String> = spec.y.iter().map(|&c| schema.field(c).name().clone()).collect();

        if spec.agg == Agg::None {
            // Raw points in view order.
            let take = n.min(spec.limit.max(1) * 20).min(50_000);
            let x = (0..take).map(|i| label(&xs, i)).collect();
            let series = nums
                .iter()
                .zip(&ynames)
                .map(|(a, name)| ChartSeries { name: name.clone(), values: (0..take).map(|i| (!a.is_null(i)).then(|| a.value(i))).collect() })
                .collect();
            return Ok(ChartData { x, x_family: family_str(xfam).into(), series, truncated: take < n, rows: n });
        }

        // Group: x → (series key → accumulators per y).
        let mut order: Vec<String> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut series_order: Vec<String> = Vec::new();
        let mut series_index: HashMap<String, usize> = HashMap::new();
        let ny = nums.len().max(1);
        // accs[x][series][y]
        let mut accs: Vec<Vec<Vec<Acc>>> = Vec::new();
        for i in 0..n {
            let xk = label(&xs, i);
            let xi = *index.entry(xk.clone()).or_insert_with(|| {
                order.push(xk);
                accs.push(Vec::new());
                order.len() - 1
            });
            let si = match &series_col {
                Some(sc) => {
                    let sk = label(sc, i);
                    *series_index.entry(sk.clone()).or_insert_with(|| {
                        series_order.push(sk);
                        series_order.len() - 1
                    })
                }
                None => 0,
            };
            let row = &mut accs[xi];
            if row.len() <= si {
                row.resize(si + 1, vec![Acc::default(); ny]);
            }
            if nums.is_empty() {
                row[si][0].add(None);
            } else {
                for (yi, a) in nums.iter().enumerate() {
                    row[si][yi].add((!a.is_null(i)).then(|| a.value(i)));
                }
            }
        }
        let mut truncated = false;
        // Keep at most MAX_SERIES series (by total).
        let mut keep_series: Vec<usize> = (0..series_order.len().max(1)).collect();
        if series_col.is_some() && series_order.len() > MAX_SERIES {
            let total = |s: usize| accs.iter().filter_map(|r| r.get(s)).map(|a| a[0].get(spec.agg).unwrap_or(0.0).abs()).sum::<f64>();
            keep_series.sort_by(|a, b| total(*b).total_cmp(&total(*a)));
            keep_series.truncate(MAX_SERIES);
            keep_series.sort();
            truncated = true;
        }
        // X order: natural sort for ordered types; categories keep the view
        // order, cut to the largest when over the limit.
        let mut xs_idx: Vec<usize> = (0..order.len()).collect();
        let ordered = matches!(xfam, TypeFamily::Number | TypeFamily::Date | TypeFamily::Time);
        if ordered && view.sort.is_empty() {
            if xfam == TypeFamily::Number {
                xs_idx.sort_by(|a, b| order[*a].parse::<f64>().unwrap_or(f64::NAN).total_cmp(&order[*b].parse::<f64>().unwrap_or(f64::NAN)));
            } else {
                xs_idx.sort_by(|a, b| order[*a].cmp(&order[*b]));
            }
        }
        if xs_idx.len() > spec.limit.max(1) {
            truncated = true;
            if ordered {
                xs_idx.truncate(spec.limit);
            } else {
                let total = |x: usize| accs[x].iter().map(|s| s[0].get(spec.agg).unwrap_or(0.0).abs()).sum::<f64>();
                let mut by_total = xs_idx.clone();
                by_total.sort_by(|a, b| total(*b).total_cmp(&total(*a)));
                by_total.truncate(spec.limit);
                let keep: std::collections::HashSet<usize> = by_total.into_iter().collect();
                xs_idx.retain(|x| keep.contains(x));
            }
        }
        let x: Vec<String> = xs_idx.iter().map(|&i| order[i].clone()).collect();
        let mut series = Vec::new();
        if series_col.is_some() {
            for &s in &keep_series {
                series.push(ChartSeries {
                    name: series_order[s].clone(),
                    values: xs_idx.iter().map(|&xi| accs[xi].get(s).and_then(|a| a[0].get(spec.agg))).collect(),
                });
            }
        } else if nums.is_empty() {
            series.push(ChartSeries { name: "count".into(), values: xs_idx.iter().map(|&xi| accs[xi][0][0].get(Agg::Count)).collect() });
        } else {
            for (yi, name) in ynames.iter().enumerate() {
                series.push(ChartSeries { name: name.clone(), values: xs_idx.iter().map(|&xi| accs[xi].first().and_then(|s| s[yi].get(spec.agg))).collect() });
            }
        }
        Ok(ChartData { x, x_family: family_str(xfam).into(), series, truncated, rows: n })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::arrow::array::{Int64Array, RecordBatch, StringArray};
    use databrain_connector_core::arrow::datatypes::{Field, Schema};
    use std::sync::Arc;

    fn rs() -> ResultSet {
        let schema = Arc::new(Schema::new(vec![
            Field::new("country", DataType::Utf8, true),
            Field::new("year", DataType::Int64, true),
            Field::new("amount", DataType::Int64, true),
        ]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["VN", "US", "VN", "JP", "US"])),
                Arc::new(Int64Array::from(vec![2024, 2023, 2023, 2024, 2024])),
                Arc::new(Int64Array::from(vec![Some(10), Some(5), Some(7), None, Some(1)])),
            ],
        )
        .unwrap();
        let mut r = ResultSet::new("t", schema);
        r.push(b);
        r.finish(false);
        r
    }

    #[test]
    fn groups_and_aggregates() {
        let mut r = rs();
        let d = r.chart(&ViewSpec::default(), &ChartSpec { x: 0, y: vec![2], agg: Agg::Sum, series: None, limit: 10 }).unwrap();
        assert_eq!(d.x, vec!["VN", "US", "JP"]);
        assert_eq!(d.series[0].values, vec![Some(17.0), Some(6.0), None]);
        let d = r.chart(&ViewSpec::default(), &ChartSpec { x: 0, y: vec![], agg: Agg::Count, series: None, limit: 2 }).unwrap();
        assert_eq!((d.x.clone(), d.truncated), (vec!["VN".to_string(), "US".to_string()], true));
        // Numeric x is sorted; series split by country.
        let d = r.chart(&ViewSpec::default(), &ChartSpec { x: 1, y: vec![2], agg: Agg::Sum, series: Some(0), limit: 10 }).unwrap();
        assert_eq!(d.x, vec!["2023", "2024"]);
        assert_eq!(d.x_family, "number");
        let vn = d.series.iter().find(|s| s.name == "VN").unwrap();
        assert_eq!(vn.values, vec![Some(7.0), Some(10.0)]);
        assert!(r.chart(&ViewSpec::default(), &ChartSpec { x: 9, y: vec![], agg: Agg::Count, series: None, limit: 10 }).is_err());
    }
}
