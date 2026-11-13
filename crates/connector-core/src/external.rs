//! Tables provided from outside a database, i.e. DataBrain query outputs
//! exposed as `results.<name>` inside DuckDB sessions.
//!
//! The query engine implements [`ExternalTables`]; connectors that can host
//! them (DuckDB) receive an [`ExternalTablesSlot`] at construction and ask it
//! to resolve the names a statement references.

use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use parking_lot::RwLock;

/// Schema name under which outputs are visible.
pub const RESULTS_SCHEMA: &str = "results";

/// One resolved output, ready to be loaded as a table.
pub struct ExternalTable {
    /// Name as referenced (`r12`, `revenue`, `revenue__1` = one version back).
    pub name: String,
    /// Changes whenever the name points at different data (the result id).
    pub version_key: String,
    pub schema: SchemaRef,
    pub batches: Vec<RecordBatch>,
    /// Warning surfaced to the user when the table is used (e.g. the output
    /// was cut at the row limit, so aggregates over it may be incomplete).
    pub notice: Option<String>,
    /// The rows are on disk in these Parquet files (the output is larger
    /// than memory); `batches` is then empty. A session that may read files
    /// views them; another streams them ([`ExternalTables::for_each_batch`]).
    pub files: Vec<std::path::PathBuf>,
    /// Columns to show as `***` (set by [`OutputMasks::apply`] for rows on
    /// disk, which are masked as they are streamed).
    pub hide: Vec<bool>,
}

pub trait ExternalTables: Send + Sync {
    /// Resolve a name referenced as `results.<name>`.
    fn resolve(&self, name: &str) -> Result<ExternalTable, String>;
    /// Names that can currently be referenced (for error messages).
    fn names(&self) -> Vec<String>;
    /// Catalog view for explorers: every referenceable name with its
    /// columns, without loading data. Defaults to names only.
    fn catalog(&self) -> Vec<ExternalInfo> {
        self.names().into_iter().map(|name| ExternalInfo { name, ..Default::default() }).collect()
    }
    /// Every row of an output on disk, a batch at a time, without keeping
    /// them all (for sessions that can't read its files).
    fn for_each_batch(&self, name: &str, _f: &mut dyn FnMut(RecordBatch) -> Result<(), String>) -> Result<(), String> {
        Err(format!("results.{name} can't be streamed"))
    }
}

/// Columns of outputs to hide from a statement (the assistant's: its
/// connections' sensitive columns), by output: the result id (an
/// [`ExternalTable::version_key`]) → the column names to mask. When a
/// statement runs with masks, every output it loads must be listed (an
/// output that wasn't checked is refused), and the listed columns are loaded
/// as `***` (NULL stays NULL): a rename, cast or expression in the SQL can't
/// bring the values back, and errors can't quote them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputMasks(pub std::collections::HashMap<String, Vec<String>>);

impl OutputMasks {
    /// Whether any listed output has a column to mask.
    pub fn any(&self) -> bool {
        self.0.values().any(|c| !c.is_empty())
    }

    /// `t` with its masked columns replaced, and a version key of its own
    /// (a session never takes a masked load for an unmasked one, or the
    /// reverse). Errors when `t` isn't listed.
    pub fn apply(&self, t: ExternalTable) -> Result<ExternalTable, String> {
        let Some(cols) = self.0.get(&t.version_key) else {
            return Err(format!("results.{} was not checked for sensitive columns", t.name));
        };
        if cols.is_empty() {
            return Ok(t);
        }
        let hide: Vec<bool> = t.schema.fields().iter().map(|f| cols.iter().any(|c| c.eq_ignore_ascii_case(f.name()))).collect();
        let batches = t.batches.iter().map(|b| mask_batch(b, &hide)).collect::<Result<Vec<_>, _>>()?;
        let schema = batches.first().map(|b| b.schema()).unwrap_or_else(|| mask_schema(&t.schema, &hide));
        let mut sorted = cols.iter().map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>();
        sorted.sort();
        // Rows on disk are masked as they're streamed in.
        let hide = if t.files.is_empty() { Vec::new() } else { hide };
        Ok(ExternalTable { version_key: format!("{}#masked:{}", t.version_key, sorted.join(",")), schema, batches, hide, ..t })
    }
}

fn mask_schema(s: &SchemaRef, hide: &[bool]) -> SchemaRef {
    use arrow::datatypes::{DataType, Field, Schema};
    let fields: Vec<Field> =
        s.fields().iter().zip(hide).map(|(f, h)| if *h { Field::new(f.name(), DataType::Utf8, true) } else { f.as_ref().clone() }).collect();
    Arc::new(Schema::new(fields))
}

/// `b` with the columns where `hide` is true replaced by `***` text (NULL
/// stays NULL, so counts of non-empty values still work).
pub fn mask_batch(b: &RecordBatch, hide: &[bool]) -> Result<RecordBatch, String> {
    use arrow::array::{Array, ArrayRef, StringArray};
    if !hide.iter().any(|h| *h) {
        return Ok(b.clone());
    }
    let cols: Vec<ArrayRef> = b
        .columns()
        .iter()
        .zip(hide.iter().chain(std::iter::repeat(&false)))
        .map(|(a, h)| if *h { Arc::new((0..a.len()).map(|r| (!a.is_null(r)).then_some("***")).collect::<StringArray>()) as ArrayRef } else { a.clone() })
        .collect();
    let hide: Vec<bool> = (0..b.num_columns()).map(|i| hide.get(i).copied().unwrap_or(false)).collect();
    RecordBatch::try_new(mask_schema(&b.schema(), &hide), cols).map_err(|e| e.to_string())
}

/// Explorer metadata of an external table (no data).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExternalInfo {
    pub name: String,
    /// (column, type) pairs.
    pub columns: Vec<(String, String)>,
    pub rows: Option<i64>,
    /// Shown as the table comment (source connection, SQL…).
    pub comment: Option<String>,
}

/// Late-bound holder: connectors are built before the engine that implements
/// [`ExternalTables`], so the engine fills the slot after construction.
#[derive(Default)]
pub struct ExternalTablesSlot(RwLock<Option<Arc<dyn ExternalTables>>>);

impl ExternalTablesSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn set(&self, t: Arc<dyn ExternalTables>) {
        *self.0.write() = Some(t);
    }
    pub fn get(&self) -> Option<Arc<dyn ExternalTables>> {
        self.0.read().clone()
    }
}

/// Names referenced as `results.<name>` / `results."<name>"` in `sql`
/// (case-insensitive schema; strings, comments and dollar-quoted text are
/// skipped as DuckDB reads them, [`crate::sql::skip_token`]). Unique, in
/// order of first appearance.
pub fn referenced_results(sql: &str) -> Vec<String> {
    let syntax = &crate::traits::DUCKDB.syntax;
    let b = sql.as_bytes();
    let n = b.len();
    // Identifier bytes: ASCII word characters and any UTF-8 byte.
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let ws = |mut j: usize| {
        while j < n && b[j].is_ascii_whitespace() {
            j += 1;
        }
        j
    };
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < n {
        // The schema: `results` or `"results"`, not part of a longer name.
        let schema_end = if b[i] == b'"' && sql.get(i + 1..i + 9).is_some_and(|w| w.eq_ignore_ascii_case("results\"")) {
            Some(i + 9)
        } else if (i == 0 || !(ident(b[i - 1]) || b[i - 1] == b'.'))
            && sql.get(i..i + 7).is_some_and(|w| w.eq_ignore_ascii_case("results"))
            && b.get(i + 7).is_none_or(|a| !ident(*a))
        {
            Some(i + 7)
        } else {
            None
        };
        let Some(schema_end) = schema_end else {
            i = match crate::sql::skip_token(sql, i, syntax) {
                Some((end, _)) => end,
                None if ident(b[i]) => (i..n).find(|&k| !ident(b[k])).unwrap_or(n),
                None => i + 1,
            };
            continue;
        };
        let mut j = ws(schema_end);
        if j >= n || b[j] != b'.' {
            i = schema_end;
            continue;
        }
        j = ws(j + 1);
        let name = if j < n && b[j] == b'"' {
            // `"…"` with `""` for a quote.
            let mut name = String::new();
            let mut k = j + 1;
            let mut from = k;
            while k < n {
                if b[k] == b'"' {
                    name.push_str(&sql[from..k]);
                    if b.get(k + 1) == Some(&b'"') {
                        name.push('"');
                        k += 2;
                        from = k;
                        continue;
                    }
                    break;
                }
                k += 1;
            }
            if k >= n {
                name.push_str(&sql[from..n]);
            }
            j = (k + 1).min(n);
            name
        } else {
            let start = j;
            while j < n && ident(b[j]) {
                j += 1;
            }
            sql[start..j].to_string()
        };
        if !name.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_references() {
        let sql = "select * from results.r12 a join RESULTS.\"revenue__1\" b using (id) -- results.nope\n\
                   where x = 'results.skip' and y in (select v from results . sales_2024) /* results.c */ \
                   union all select * from results.r12, myresults.x, results_tbl.y, \"results\".\"Big Name\"";
        assert_eq!(referenced_results(sql), vec!["r12", "revenue__1", "sales_2024", "Big Name"]);
        assert!(referenced_results("select 'it''s results.x'").is_empty());
        assert_eq!(referenced_results("SELECT * FROM results.revenue__2 r"), vec!["revenue__2"]);
        assert!(referenced_results("select results from t").is_empty());
        // Dollar-quoted and E'…' text is skipped as DuckDB reads it.
        assert!(referenced_results("select $$ results.a $$, E'it\\'s results.b'").is_empty());
        assert_eq!(referenced_results("select * from results.ünï, results.\"a\"\"b\""), vec!["ünï", "a\"b"]);
    }
}

#[cfg(test)]
mod mask_tests {
    use super::*;
    use arrow::array::{Array, Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};

    fn table() -> ExternalTable {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false), Field::new("Email", DataType::Utf8, true)]));
        let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int32Array::from(vec![1, 2])), Arc::new(StringArray::from(vec![Some("a@x.io"), None]))])
            .unwrap();
        ExternalTable { name: "people".into(), version_key: "r1".into(), schema, batches: vec![b], notice: None, files: vec![], hide: vec![] }
    }

    #[test]
    fn masks_listed_columns_and_refuses_unlisted_outputs() {
        let m = OutputMasks([("r1".to_string(), vec!["email".to_string()])].into());
        let t = m.apply(table()).unwrap();
        assert_ne!(t.version_key, "r1", "a masked load is not the unmasked one");
        let col = t.batches[0].column(1).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!((col.value(0), col.is_null(1)), ("***", true));
        assert_eq!(t.batches[0].column(0).len(), 2);
        // Listed with nothing to mask: unchanged.
        let none = OutputMasks([("r1".to_string(), vec![])].into());
        assert_eq!(none.apply(table()).unwrap().version_key, "r1");
        // Not listed: refused.
        assert!(OutputMasks::default().apply(table()).is_err());
    }
}
