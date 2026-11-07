use serde::Serialize;

use super::splitter::{dollar_tag, lex, skip_line, skip_quoted};
use crate::traits::DollarQuotes;
use crate::types::ConnectorKind;

/// A parameter in a script (byte offsets of `:name` / `&name`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct ParamSpan {
    pub name: String,
    pub start: usize,
    pub end: usize,
    /// Oracle substitution variable (`&name`, `&&name`, SQL*Plus style): the
    /// value replaces the text as typed (also inside `'…'`), not as a literal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub textual: bool,
}

/// `&name` / `&&name` (an optional `.` after the name ends it, as in
/// SQL*Plus) in `b[from..to]`.
fn ampersand_vars(text: &str, from: usize, to: usize, out: &mut Vec<ParamSpan>) {
    let mut i = from;
    while i < to {
        match ampersand_at(text, i, to) {
            Some(p) => {
                i = p.end;
                out.push(p);
            }
            None => i += 1,
        }
    }
}

/// The substitution variable starting at `i`, if any.
fn ampersand_at(text: &str, i: usize, to: usize) -> Option<ParamSpan> {
    let b = text.as_bytes();
    if b.get(i) != Some(&b'&') {
        return None;
    }
    let mut j = i + 1;
    if j < to && b[j] == b'&' {
        j += 1;
    }
    if !(j < to && (b[j].is_ascii_alphabetic() || b[j] == b'_')) {
        return None;
    }
    let name_start = j;
    while j < to && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$' || b[j] == b'#') {
        j += 1;
    }
    let name = text[name_start..j].to_string();
    if j < to && b[j] == b'.' {
        j += 1;
    }
    Some(ParamSpan { name, start: i, end: j, textual: true })
}

/// `:name` parameters outside strings, quoted identifiers and comments.
/// Not parameters: `::type` casts, `col:field` / `$1:field` paths (a name,
/// digit, quote or bracket right before the colon), `:=` assignments, and
/// Oracle trigger `:new` / `:old`. Oracle also has SQL*Plus substitution
/// variables `&name` / `&&name`, in code and inside `'…'`.
pub fn find_parameters(text: &str, kind: ConnectorKind) -> Vec<ParamSpan> {
    if !kind.traits().syntax.colon_params {
        return vec![];
    }
    let amp = kind.traits().syntax.ampersand_vars;
    let b = text.as_bytes();
    let n = b.len();
    let lx = lex(kind);
    let mut out = Vec::new();
    let mut i = 0usize;
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < n {
        let c = b[i];
        match c {
            b'&' if amp => match ampersand_at(text, i, n) {
                Some(p) => {
                    i = p.end;
                    out.push(p);
                }
                None => i += 1,
            },
            b'-' if i + 1 < n && b[i + 1] == b'-' => i = skip_line(b, i),
            b'#' if lx.hash_comment => i = skip_line(b, i),
            b'/' if lx.slash_comment && i + 1 < n && b[i + 1] == b'/' => i = skip_line(b, i),
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i = text[i + 2..].find("*/").map_or(n, |p| i + 2 + p + 2);
            }
            b'\'' | b'"' if lx.triple_quotes && b[i..].starts_with(if c == b'"' { b"\"\"\"" } else { b"'''" }) => {
                let q = &text[i..i + 3];
                i = text[i + 3..].find(q).map_or(n, |p| i + 3 + p + 3);
            }
            b'\'' => {
                let end = skip_quoted(b, i, b'\'', lx.backslash_escapes);
                if amp {
                    ampersand_vars(text, i + 1, end.saturating_sub(1).max(i + 1), &mut out);
                }
                i = end;
            }
            b'"' => i = skip_quoted(b, i, b'"', kind.traits().syntax.backslash_in_double_quotes),
            b'`' if lx.backtick => i = skip_quoted(b, i, b'`', false),
            b'[' if lx.brackets => i = text[i..].find(']').map_or(n, |p| i + p + 1),
            b'$' if lx.dollar => match dollar_tag(text, i) {
                Some(tag) if kind.traits().syntax.dollar == DollarQuotes::Tagged || tag == "$$" => {
                    let body = i + tag.len();
                    i = text[body..].find(tag).map_or(n, |p| body + p + tag.len());
                }
                _ => i += 1,
            },
            b':' => {
                if i + 1 < n && b[i + 1] == b':' {
                    i += 2;
                    continue;
                }
                let attached = i > 0 && (word(b[i - 1]) || matches!(b[i - 1], b'.' | b']' | b'"' | b'`' | b'\'' | b')' | b'$'));
                let starts = i + 1 < n && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_');
                if attached || !starts {
                    i += 1;
                    continue;
                }
                let mut j = i + 1;
                while j < n && word(b[j]) {
                    j += 1;
                }
                let name = &text[i + 1..j];
                let trigger_row = kind.traits().syntax.trigger_row_binds && (name.eq_ignore_ascii_case("new") || name.eq_ignore_ascii_case("old"));
                if !trigger_row {
                    out.push(ParamSpan { name: name.to_string(), start: i, end: j, textual: false });
                }
                i = j;
            }
            _ => i += 1,
        }
    }
    out
}

/// A value typed for a parameter.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct ParamValue {
    pub value: String,
    /// Insert as typed (an SQL expression such as `current_date`), no quoting.
    #[serde(default)]
    #[ts(as = "Option<bool>", optional)]
    pub raw: bool,
}

/// The SQL that replaces a parameter. Numbers (`2000`, `-1.5`, `1e3`),
/// `NULL`/`TRUE`/`FALSE` and complete string literals (`'2026-09-09'`) are
/// kept; anything else becomes a string literal (`2026-09-09` →
/// `'2026-09-09'`), escaped for the dialect. Numbers with a leading zero
/// (`007`, codes) are strings. `None` when the value is empty.
pub fn render_param(v: &ParamValue, kind: ConnectorKind) -> Option<String> {
    let t = v.value.trim();
    if t.is_empty() {
        return None;
    }
    if v.raw || is_number(t) || ["null", "true", "false"].iter().any(|k| t.eq_ignore_ascii_case(k)) || is_string_literal(t, kind) {
        return Some(t.to_string());
    }
    Some(quote_string(t, kind))
}

fn is_number(t: &str) -> bool {
    let s = t.strip_prefix(['-', '+']).unwrap_or(t);
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(p) => (&s[..p], Some(&s[p + 1..])),
        None => (s, None),
    };
    let (int, frac) = match mant.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (mant, None),
    };
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit());
    let int_ok = digits(int) || (int.is_empty() && frac.is_some_and(digits));
    let frac_ok = frac.is_none_or(|f| f.is_empty() && !int.is_empty() || digits(f));
    let exp_ok = exp.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)));
    // `007` is a code, not a number; `0`, `0.5` are numbers.
    let leading_zero = int.len() > 1 && int.starts_with('0');
    int_ok && frac_ok && exp_ok && !leading_zero
}

/// One complete `'…'` literal (optionally `N'…'` / `E'…'`), kept as typed.
/// A literal with a backslash isn't: where its end depends on whether the
/// server reads backslashes as escapes (MySQL `NO_BACKSLASH_ESCAPES`), the
/// typed text is quoted as a value instead.
fn is_string_literal(t: &str, kind: ConnectorKind) -> bool {
    if lex(kind).backslash_escapes && t.contains('\\') {
        return false;
    }
    let b = t.as_bytes();
    let start = match b.first() {
        Some(b'\'') => 0,
        Some(b'N' | b'n' | b'E' | b'e') if b.get(1) == Some(&b'\'') => 1,
        _ => return false,
    };
    b.len() >= start + 2 && skip_quoted(b, start, b'\'', lex(kind).backslash_escapes) == b.len() && b[b.len() - 1] == b'\''
}

/// `'text'` with the dialect's escaping: the one text escaper (parameters,
/// load INSERTs, SQL export). Where backslashes may escape, they are
/// doubled, so a value ending in `\` can't escape the closing quote.
/// Quotes are doubled where the dialect accepts it, so the literal ends at
/// the same place whether or not the server reads backslashes as escapes
/// (MySQL `NO_BACKSLASH_ESCAPES`); BigQuery and Databricks only accept `\'`.
pub fn quote_string(t: &str, kind: ConnectorKind) -> String {
    if !lex(kind).backslash_escapes {
        return format!("'{}'", t.replace('\'', "''"));
    }
    let t = t.replace('\\', "\\\\");
    match kind {
        ConnectorKind::Bigquery | ConnectorKind::Databricks => format!("'{}'", t.replace('\'', "\\'")),
        _ => format!("'{}'", t.replace('\'', "''")),
    }
}

/// Replace every parameter of `sql`. Errors with the names that have no value.
pub fn substitute_parameters(sql: &str, kind: ConnectorKind, values: &std::collections::HashMap<String, ParamValue>) -> Result<String, Vec<String>> {
    let spans = find_parameters(sql, kind);
    if spans.is_empty() {
        return Ok(sql.to_string());
    }
    let mut missing: Vec<String> = Vec::new();
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    for s in &spans {
        let lit = values.get(&s.name).and_then(|v| match s.textual {
            // SQL*Plus: the text as typed.
            true => Some(v.value.clone()).filter(|t| !t.trim().is_empty()),
            false => render_param(v, kind),
        });
        match lit {
            Some(lit) => {
                out.push_str(&sql[at..s.start]);
                out.push_str(&lit);
                at = s.end;
            }
            None if !missing.contains(&s.name) => missing.push(s.name.clone()),
            None => {}
        }
    }
    if !missing.is_empty() {
        return Err(missing);
    }
    out.push_str(&sql[at..]);
    Ok(out)
}

#[cfg(test)]
mod param_tests {
    use super::*;
    use ConnectorKind as K;
    use std::collections::HashMap;

    /// A rendered value is one literal that ends at its last character
    /// whether or not the server reads backslashes as escapes (MySQL
    /// `NO_BACKSLASH_ESCAPES`), for hostile values and typed literals alike.
    #[test]
    fn values_stay_inside_their_quotes() {
        let ends_at_last = |lit: &str, backslash: bool| {
            let b = lit.as_bytes();
            let start = usize::from(b[0] != b'\'');
            skip_quoted(b, start, b'\'', backslash) == b.len()
        };
        let hostile = ["x\\' or 1=1 --", "a\\", "it's", "\\\\'; drop table t; --", "'x\\' or 1=1 --'", "plain"];
        for kind in [K::Mysql, K::Clickhouse, K::Snowflake, K::Postgres, K::Mssql] {
            for v in hostile {
                let lit = render_param(&ParamValue { value: v.into(), raw: false }, kind).unwrap();
                // Engines that may read backslashes are checked both ways.
                let both = !lex(kind).backslash_escapes || ends_at_last(&lit, true);
                assert!(both && ends_at_last(&lit, false), "{kind:?}: {v} → {lit}");
            }
        }
        // BigQuery and Databricks always read backslashes (and don't accept '').
        for kind in [K::Bigquery, K::Databricks] {
            for v in hostile {
                let lit = render_param(&ParamValue { value: v.into(), raw: false }, kind).unwrap();
                assert!(ends_at_last(&lit, true) && !lit[1..lit.len() - 1].contains("''"), "{kind:?}: {v} → {lit}");
            }
            assert_eq!(quote_string("it's", kind), "'it\\'s'");
        }
        assert_eq!(quote_string("a\\b'c", K::Mysql), "'a\\\\b''c'");
        // A typed literal is kept unless its end depends on the backslash mode.
        assert_eq!(render_param(&ParamValue { value: "'2026-09-09'".into(), raw: false }, K::Mysql).as_deref(), Some("'2026-09-09'"));
        assert_ne!(render_param(&ParamValue { value: "'x\\' or 1=1 --'".into(), raw: false }, K::Mysql).as_deref(), Some("'x\\' or 1=1 --'"));
    }

    fn names(sql: &str, k: K) -> Vec<String> {
        find_parameters(sql, k).into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn finds_parameters_in_code_only() {
        assert_eq!(names("select * from t where d = :data_date and n > :min_n", K::Postgres), vec!["data_date", "min_n"]);
        assert_eq!(names("select x::date, ':nope', \"a:b\" -- :c\n /* :d */ from t where y = :p", K::Postgres), vec!["p"]);
        assert_eq!(names("select $$ :x $$, $f$ :y $f$, :z", K::Postgres), vec!["z"]);
        assert_eq!(names("select src:customer.name, $1:field, t.c:path from s where a = :p", K::Snowflake), vec!["p"]);
        assert_eq!(names("set @a := 1; select `x:y`, :p # :q", K::Mysql), vec!["p"]);
        assert_eq!(names("select [a:b], :p from t", K::Mssql), vec!["p"]);
        assert_eq!(names("select '12:30', 12:30, :p1, :_x, :1 from dual", K::Oracle), vec!["p1", "_x"]);
        assert_eq!(names("create trigger t before insert on x for each row begin :new.id := :seq; end;", K::Oracle), vec!["seq"]);
        let p = &find_parameters("a = :name", K::Sqlite)[0];
        assert_eq!((p.start, p.end), (4, 9));
    }

    #[test]
    fn oracle_substitution_variables() {
        let spans = find_parameters("select * from emp where deptno = &dept and ename = '&&who' and x = 'Tom & Jerry' -- &c\n and t = '&tbl._x'", K::Oracle);
        let v: Vec<(&str, bool)> = spans.iter().map(|p| (p.name.as_str(), p.textual)).collect();
        assert_eq!(v, vec![("dept", true), ("who", true), ("tbl", true)]);
        assert_eq!(names("select a & b from t", K::Postgres), Vec::<String>::new(), "only Oracle");
        let mut vals = HashMap::new();
        vals.insert("dept".to_string(), ParamValue { value: "10".into(), raw: false });
        vals.insert("who".to_string(), ParamValue { value: "SMITH".into(), raw: false });
        vals.insert("tbl".to_string(), ParamValue { value: "emp".into(), raw: false });
        let sql = "select * from emp where deptno = &dept and ename = '&&who' and t = '&tbl._x' and n = :n";
        vals.insert("n".to_string(), ParamValue { value: "O'Hara".into(), raw: false });
        assert_eq!(
            substitute_parameters(sql, K::Oracle, &vals).unwrap(),
            "select * from emp where deptno = 10 and ename = 'SMITH' and t = 'emp_x' and n = 'O''Hara'",
            "as typed, inside quotes too; `.` ends the name"
        );
    }

    #[test]
    fn renders_values_by_type() {
        let r = |v: &str, k: K| render_param(&ParamValue { value: v.into(), raw: false }, k);
        assert_eq!(r("2026-09-09", K::Postgres).as_deref(), Some("'2026-09-09'"));
        assert_eq!(r("'2026-09-09'", K::Postgres).as_deref(), Some("'2026-09-09'"), "already a literal");
        assert_eq!(r(" 2000 ", K::Postgres).as_deref(), Some("2000"));
        for n in ["-1.5", "0", "0.25", ".5", "1e6", "3.", "+7"] {
            assert_eq!(r(n, K::Postgres).as_deref(), Some(n), "{n}");
        }
        assert_eq!(r("007", K::Postgres).as_deref(), Some("'007'"), "codes keep their zeros");
        assert_eq!(r("1.2.3", K::Postgres).as_deref(), Some("'1.2.3'"));
        assert_eq!(r("NULL", K::Postgres).as_deref(), Some("NULL"));
        assert_eq!(r("O'Brien", K::Postgres).as_deref(), Some("'O''Brien'"));
        assert_eq!(r("O'Brien", K::Bigquery).as_deref(), Some("'O\\'Brien'"));
        assert_eq!(r("a\\b", K::Mysql).as_deref(), Some("'a\\\\b'"));
        assert_eq!(r("'it''s'", K::Postgres).as_deref(), Some("'it''s'"));
        assert_eq!(r("'a' or '1'='1'", K::Postgres).as_deref(), Some("'''a'' or ''1''=''1'''"), "not one literal: quoted whole");
        assert_eq!(r("N'x'", K::Mssql).as_deref(), Some("N'x'"));
        assert_eq!(r("   ", K::Postgres), None);
        assert_eq!(render_param(&ParamValue { value: "current_date - 1".into(), raw: true }, K::Postgres).as_deref(), Some("current_date - 1"));
    }

    #[test]
    fn substitutes_or_reports_missing() {
        let mut v = HashMap::new();
        v.insert("d".to_string(), ParamValue { value: "2026-09-09".into(), raw: false });
        v.insert("n".to_string(), ParamValue { value: "2000".into(), raw: false });
        assert_eq!(substitute_parameters("select :d, :n, ':d', :d", K::Postgres, &v).unwrap(), "select '2026-09-09', 2000, ':d', '2026-09-09'");
        assert_eq!(substitute_parameters("select :d, :x, :y, :x", K::Postgres, &v).unwrap_err(), vec!["x", "y"]);
        assert_eq!(substitute_parameters("select 1", K::Postgres, &HashMap::new()).unwrap(), "select 1");
    }
}
