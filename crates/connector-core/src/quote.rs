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

/// Quote a string literal with single quotes.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
