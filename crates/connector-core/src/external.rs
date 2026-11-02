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
        Ok(ExternalTable { version_key: format!("{}#masked:{}", t.version_key, sorted.join(",")), schema, batches, ..t })
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
/// (case-insensitive schema; strings and comments are skipped). Unique, in
/// order of first appearance.
pub fn referenced_results(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let ident_char = |c: char| c.is_alphanumeric() || c == '_';
    while i < n {
        let c = chars[i];
        // Skip string literals, comments and dollar quotes.
        if c == '\'' {
            i += 1;
            while i < n {
                if chars[i] == '\'' {
                    if i + 1 < n && chars[i + 1] == '\'' {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == '-' && i + 1 < n && chars[i + 1] == '-' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        // Candidate schema token: `results` or `"results"`, not part of a longer identifier.
        let (schema_end, matched) = if c == '"' {
            let word: String = chars[i + 1..].iter().take(8).collect();
            if word.eq_ignore_ascii_case("results\"") { (i + 9, true) } else { (i + 1, false) }
        } else if (i == 0 || !ident_char(chars[i - 1]) && chars[i - 1] != '.') && c.eq_ignore_ascii_case(&'r') {
            let word: String = chars[i..].iter().take(7).collect();
            let after = chars.get(i + 7).copied();
            if word.eq_ignore_ascii_case("results") && after.is_none_or(|a| !ident_char(a)) { (i + 7, true) } else { (i + 1, false) }
        } else {
            (i + 1, false)
        };
        if !matched {
            if c == '"' {
                // Skip a quoted identifier entirely.
                let mut j = i + 1;
                while j < n && chars[j] != '"' {
                    j += 1;
                }
                i = j + 1;
            } else {
                i = schema_end;
            }
            continue;
        }
        let mut j = schema_end;
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }
        if j >= n || chars[j] != '.' {
            i = schema_end;
            continue;
        }
        j += 1;
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }
        let name = if j < n && chars[j] == '"' {
            let start = j + 1;
            let mut k = start;
            let mut s = String::new();
            while k < n {
                if chars[k] == '"' {
                    if k + 1 < n && chars[k + 1] == '"' {
                        s.push('"');
                        k += 2;
                        continue;
                    }
                    break;
                }
                s.push(chars[k]);
                k += 1;
            }
            j = k + 1;
            s
        } else {
            let start = j;
            while j < n && ident_char(chars[j]) {
                j += 1;
            }
            chars[start..j].iter().collect()
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
        ExternalTable { name: "people".into(), version_key: "r1".into(), schema, batches: vec![b], notice: None }
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
