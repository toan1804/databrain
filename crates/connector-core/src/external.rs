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
