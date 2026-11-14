//! Quoting identifiers, paths and literals for a dialect, and splitting a
//! typed path back into its parts.

use crate::traits;
use crate::types::ConnectorKind;

/// Quote an identifier for the dialect.
pub fn quote_ident(kind: ConnectorKind, ident: &str) -> String {
    use traits::IdentQuote as Q;
    match kind.traits().quote {
        Q::Backtick => format!("`{}`", ident.replace('`', "``")),
        // Backslash first: a name ending in `\` must not escape the closing quote.
        Q::BacktickBackslash => format!("`{}`", ident.replace('\\', "\\\\").replace('`', "\\`")),
        Q::Bracket => format!("[{}]", ident.replace(']', "]]")),
        Q::Double => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

/// Quote a possibly qualified name (`catalog.schema` stored as one string,
/// or a path as typed: parts already quoted are not quoted twice).
pub fn quote_path(kind: ConnectorKind, path: &str) -> String {
    split_path(kind, path).iter().map(|p| quote_ident(kind, p)).collect::<Vec<_>>().join(".")
}

/// `schema.name` for an object from the catalog: `schema` may hold several
/// parts (`catalog.schema` stored as one string) and is split like
/// [`quote_path`], but `name` is one identifier as the database reported it,
/// so a table named `a.b` stays one name. Empty `schema`: just the name.
pub fn quote_object(kind: ConnectorKind, schema: &str, name: &str) -> String {
    if schema.trim().is_empty() {
        return quote_ident(kind, name);
    }
    format!("{}.{}", quote_path(kind, schema), quote_ident(kind, name))
}

/// The parts of a qualified name as typed: dots outside quotes split it, a
/// part may be quoted (`"My Schema".t`, `[dbo].[t]`, `` `a`.b ``, doubled
/// quotes inside), quotes are removed. BigQuery's whole-path backticks
/// (`` `project.dataset.table` ``) are split inside too. Unquoted parts are
/// trimmed and kept as written.
pub fn split_path(kind: ConnectorKind, path: &str) -> Vec<String> {
    if path.trim().is_empty() {
        return vec![];
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    // Closing quote of the part being read, and whether the part was quoted.
    let mut close: Option<char> = None;
    let mut quoted = false;
    let mut chars = path.chars().peekable();
    let push = |parts: &mut Vec<String>, cur: &mut String, quoted: bool| {
        let p = std::mem::take(cur);
        if quoted && p.contains('.') && kind.traits().whole_path_quotes {
            parts.extend(p.split('.').map(str::to_string));
        } else {
            parts.push(if quoted { p } else { p.trim().to_string() });
        }
    };
    let backslash = kind.traits().quote == traits::IdentQuote::BacktickBackslash;
    while let Some(c) = chars.next() {
        match close {
            // `\` escapes the next character inside backticks (ClickHouse, BigQuery).
            Some('`') if backslash && c == '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            Some(q) if c == q => {
                if chars.peek() == Some(&q) {
                    cur.push(q);
                    chars.next();
                } else {
                    close = None;
                }
            }
            Some(_) => cur.push(c),
            None if c == '.' => {
                push(&mut parts, &mut cur, quoted);
                quoted = false;
            }
            // A quote opens only at the start of a part.
            None if cur.trim().is_empty() && !quoted && matches!(c, '"' | '`' | '[') => {
                cur.clear();
                quoted = true;
                close = Some(if c == '[' { ']' } else { c });
            }
            None if quoted && c.is_whitespace() => {}
            None => cur.push(c),
        }
    }
    push(&mut parts, &mut cur, quoted);
    parts
}

/// Quote a string literal with single quotes, for engines where a
/// backslash in a literal is an ordinary character (DuckDB, SQLite, SQL
/// Server, Oracle). Where it may be an escape, `\'` would end the literal
/// early and the rest of the value would be read as SQL: MySQL, Snowflake,
/// Databricks, BigQuery and ClickHouse take
/// [`quote_string`](crate::sql::quote_string) with their kind; Postgres
/// and Redshift take [`quote_literal_pg`].
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A string literal for Postgres and Redshift that means the same text
/// whether or not the server reads backslashes as escapes (Redshift does;
/// Postgres does with `standard_conforming_strings = off`): the literal
/// holds no backslash, each one is spliced in as `chr(92)`.
pub fn quote_literal_pg(s: &str) -> String {
    let quoted = |part: &str| format!("'{}'", part.replace('\'', "''"));
    if !s.contains('\\') {
        return quoted(s);
    }
    format!("({})", s.split('\\').map(quoted).collect::<Vec<_>>().join("||chr(92)||"))
}

#[cfg(test)]
mod literal_tests {
    use super::*;

    /// A value with a backslash before a quote can't end a Postgres or
    /// Redshift literal early, whichever way the server reads backslashes.
    #[test]
    fn pg_literals_hold_no_backslash() {
        assert_eq!(quote_literal_pg("orders"), "'orders'");
        assert_eq!(quote_literal_pg("o'brien"), "'o''brien'");
        assert_eq!(quote_literal_pg(r"a\b"), "('a'||chr(92)||'b')");
        assert_eq!(quote_literal_pg(r"x\' union select 1 --"), "('x'||chr(92)||''' union select 1 --')");
        assert_eq!(quote_literal_pg(r"\"), "(''||chr(92)||'')");
        for v in [r"x\' or 1=1 --", r"trailing\", r"\\'", "plain"] {
            let lit = quote_literal_pg(v);
            assert!(!lit.contains('\\'), "{lit}");
            // Every quote inside the literal is doubled: an even count in all.
            assert_eq!(lit.matches('\'').count() % 2, 0, "{lit}");
        }
    }
}
