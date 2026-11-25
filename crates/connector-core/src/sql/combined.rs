//! Combined queries: one DuckDB `SELECT` whose `WITH` list has at least one
//! **part**, a CTE whose body starts with `@connection` and runs on that
//! connection (docs/design/cross-connection-queries.md, §2):
//!
//! ```sql
//! WITH orders AS (@"Sales PG" SELECT id, total FROM orders),
//!      customers AS (@crm SELECT id, name FROM customers)
//! SELECT … FROM orders JOIN customers …
//! ```
//!
//! The outer query is read with DuckDB's lexer; a part's body with its own
//! connection's, so a `)` in one of its strings or comments doesn't end it.
//! Only the statement's own `WITH` list is read: a part nested in a
//! subquery isn't one (it fails in DuckDB, as `@` isn't SQL there).

use std::ops::Range;

use super::splitter::{Skipped, skip_token};
use crate::traits::{QueryLanguage, SqlSyntax};
use crate::types::ConnectorKind;

/// How a part names its connection: `@name`, `@"a name"` or `@id:<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConnectionRef {
    Name(String),
    Id(String),
}

impl std::fmt::Display for ConnectionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionRef::Name(n) => write!(f, "{n}"),
            ConnectionRef::Id(i) => write!(f, "id:{i}"),
        }
    }
}

/// One part of a combined query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinedPart {
    /// The CTE's name (unquoted), which the outer query reads.
    pub name: String,
    pub connection: ConnectionRef,
    /// The connection's kind (its dialect read the part).
    pub kind: ConnectorKind,
    /// The part's SQL, as written (trimmed).
    pub sql: String,
    /// Where [`Self::sql`] is in the text (error positions).
    pub sql_span: Range<usize>,
    /// The CTE's body, `(` to `)` inclusive: what the outer query reads
    /// instead ([`Combined::outer_sql`]).
    pub body_span: Range<usize>,
}

/// A combined query's parts, in the order written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combined {
    pub parts: Vec<CombinedPart>,
}

/// Why a statement that has a part can't run: `position` is a byte offset
/// in the statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinedError {
    pub message: String,
    pub position: usize,
}

impl Combined {
    /// The outer query (DuckDB): `text` with each part's body replaced by
    /// `read(part)` (`(SELECT * FROM results.r4)`). Everything else is
    /// left as written.
    pub fn outer_sql(&self, text: &str, mut read: impl FnMut(&CombinedPart) -> String) -> String {
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for p in &self.parts {
            out.push_str(&text[at..p.body_span.start]);
            out.push_str(&read(p));
            at = p.body_span.end;
        }
        out.push_str(&text[at..]);
        out
    }
}

/// Cheap test, before [`parse_combined`]: whether `text` could be a
/// combined query (it starts with `WITH` and has an `@`).
pub fn looks_combined(text: &str) -> bool {
    if !text.contains('@') {
        return false;
    }
    let syn = &ConnectorKind::Duckdb.traits().syntax;
    let i = trivia(text, 0, syn);
    word(text, i).is_some_and(|(w, _)| w.eq_ignore_ascii_case("with"))
}

/// The parts of `text` when it is a combined query; `None` when it isn't
/// (no CTE of its `WITH` list starts with `@`), which leaves the statement
/// as it was. `resolve` gives each part's connection kind, or why there is
/// none (an unknown name: its message is shown). Once a part is found, what
/// doesn't read as one is an error rather than `None`: the statement can't
/// run anywhere as written.
pub fn parse_combined(text: &str, resolve: &mut dyn FnMut(&ConnectionRef) -> Result<ConnectorKind, String>) -> Result<Option<Combined>, CombinedError> {
    if !looks_combined(text) {
        return Ok(None);
    }
    let syn = &ConnectorKind::Duckdb.traits().syntax;
    let mut parts: Vec<CombinedPart> = Vec::new();
    // Before a part is found, anything unexpected means "not combined".
    let fail = |parts: &[CombinedPart], message: String, position: usize| -> Result<Option<Combined>, CombinedError> {
        if parts.is_empty() { Ok(None) } else { Err(CombinedError { message, position }) }
    };
    let mut i = trivia(text, 0, syn);
    let Some((_, after_with)) = word(text, i).filter(|(w, _)| w.eq_ignore_ascii_case("with")) else { return Ok(None) };
    i = trivia(text, after_with, syn);
    // RECURSIVE, unless it is the first CTE's name (`with recursive as (…)`).
    if let Some((w, e)) = word(text, i)
        && w.eq_ignore_ascii_case("recursive")
        && !word(text, trivia(text, e, syn)).is_some_and(|(n, _)| n.eq_ignore_ascii_case("as"))
        && !text[trivia(text, e, syn)..].starts_with('(')
    {
        i = trivia(text, e, syn);
    }
    loop {
        // name [ ( columns ) ] AS [ [NOT] MATERIALIZED ] ( body )
        let Some((name, e)) = ident(text, i) else { return fail(&parts, "Expected the name of a part or CTE".into(), i) };
        i = trivia(text, e, syn);
        if text[i..].starts_with('(') {
            let Some(close) = close_paren(text, i, syn) else { return fail(&parts, format!("The column list of `{name}` isn't closed"), i) };
            i = trivia(text, close + 1, syn);
        }
        // DuckDB: `name USING KEY (columns) AS (…)`.
        if let Some((w, e)) = word(text, i)
            && w.eq_ignore_ascii_case("using")
            && let Some((k, e)) = word(text, trivia(text, e, syn))
            && k.eq_ignore_ascii_case("key")
        {
            i = trivia(text, e, syn);
            let Some(close) = text[i..].starts_with('(').then(|| close_paren(text, i, syn)).flatten() else {
                return fail(&parts, format!("Expected the key columns of `{name}` in parentheses"), i);
            };
            i = trivia(text, close + 1, syn);
        }
        let Some((_, e)) = word(text, i).filter(|(w, _)| w.eq_ignore_ascii_case("as")) else {
            return fail(&parts, format!("Expected AS after `{name}`"), i);
        };
        i = trivia(text, e, syn);
        if let Some((w, e)) = word(text, i)
            && w.eq_ignore_ascii_case("not")
        {
            i = trivia(text, e, syn);
        }
        if let Some((w, e)) = word(text, i)
            && w.eq_ignore_ascii_case("materialized")
        {
            i = trivia(text, e, syn);
        }
        if !text[i..].starts_with('(') {
            return fail(&parts, format!("Expected ( after `{name} AS`"), i);
        }
        let open = i;
        let inner = trivia(text, open + 1, syn);
        if text[inner..].starts_with('@') {
            parts.push(part(text, &name, open, inner, resolve)?);
            i = parts.last().expect("pushed").body_span.end;
        } else {
            let Some(close) = close_paren(text, open, syn) else { return fail(&parts, format!("`{name}` isn't closed"), open) };
            i = close + 1;
        }
        i = trivia(text, i, syn);
        if text[i..].starts_with(',') {
            i = trivia(text, i + 1, syn);
            continue;
        }
        break;
    }
    if parts.is_empty() {
        return Ok(None);
    }
    if text[i..].trim_matches(|c: char| c.is_whitespace() || c == ';').is_empty() {
        return Err(CombinedError { message: "The query that reads the parts is missing after the WITH list".into(), position: i });
    }
    Ok(Some(Combined { parts }))
}

/// The part whose body opens at `open` (`(`), its `@` at `at`.
fn part(
    text: &str,
    name: &str,
    open: usize,
    at: usize,
    resolve: &mut dyn FnMut(&ConnectionRef) -> Result<ConnectorKind, String>,
) -> Result<CombinedPart, CombinedError> {
    let err = |message: String, position: usize| CombinedError { message, position };
    let (connection, ref_end) =
        connection_ref(text, at + 1).ok_or_else(|| err(format!("Part `{name}`: name its connection after @ (@name, @\"a name\" or @id:…)"), at))?;
    let kind = resolve(&connection).map_err(|m| err(format!("Part `{name}`: {m}"), at))?;
    let traits = kind.traits();
    if traits.language != QueryLanguage::Sql {
        return Err(err(format!("Part `{name}`: {} connections can't be parts yet, only SQL ones", traits.name), at));
    }
    // From after the name (DuckDB text, read already): the part's own lexer
    // reads only the part's SQL.
    let close = close_paren_from(text, ref_end, 1, &traits.syntax).ok_or_else(|| err(format!("Part `{name}`: its closing parenthesis is missing"), open))?;
    let raw = &text[ref_end..close];
    let lead = raw.len() - raw.trim_start().len();
    let sql = raw.trim();
    if sql.is_empty() {
        return Err(err(format!("Part `{name}` has no SQL after @{connection}"), ref_end));
    }
    let start = ref_end + lead;
    Ok(CombinedPart { name: name.to_string(), connection, kind, sql: sql.to_string(), sql_span: start..start + sql.len(), body_span: open..close + 1 })
}

/// `name`, `"a ""quoted"" name"` or `id:<id>` at `i`, and where it ends.
fn connection_ref(text: &str, i: usize) -> Option<(ConnectionRef, usize)> {
    let rest = &text[i..];
    if rest.starts_with('"') {
        let (name, end) = quoted(text, i)?;
        return (!name.trim().is_empty()).then_some((ConnectionRef::Name(name), end));
    }
    // An id: `id:` (any case) and what ids are made of.
    if rest.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("id:")) {
        let len = rest[3..].find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-'))).unwrap_or(rest.len() - 3);
        return (len > 0).then(|| (ConnectionRef::Id(rest[3..3 + len].to_string()), i + 3 + len));
    }
    // A name: letters, digits and `_ - . $` (others: quote it).
    let len = rest.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '$'))).unwrap_or(rest.len());
    let token = &rest[..len];
    (!token.is_empty()).then(|| (ConnectionRef::Name(token.to_string()), i + len))
}

/// A `"…"` identifier at `i` (`""` inside is a quote), and where it ends.
fn quoted(text: &str, i: usize) -> Option<(String, usize)> {
    let b = text.as_bytes();
    let mut out = String::new();
    let mut k = i + 1;
    let mut from = k;
    while k < b.len() {
        if b[k] == b'"' {
            out.push_str(&text[from..k]);
            if b.get(k + 1) == Some(&b'"') {
                out.push('"');
                k += 2;
                from = k;
                continue;
            }
            return Some((out, k + 1));
        }
        k += 1;
    }
    None
}

/// A CTE name: a word or a `"…"` identifier.
fn ident(text: &str, i: usize) -> Option<(String, usize)> {
    if text[i..].starts_with('"') {
        return quoted(text, i).filter(|(n, _)| !n.is_empty());
    }
    let (w, e) = word(text, i)?;
    Some((w.to_string(), e))
}

/// The word at `i` (letters, digits, `_`, `$`, non-ASCII), and its end.
fn word(text: &str, i: usize) -> Option<(&str, usize)> {
    let rest = text.get(i..)?;
    let len = rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(rest.len());
    (len > 0 && !rest.starts_with(|c: char| c.is_ascii_digit() || c == '$')).then(|| (&rest[..len], i + len))
}

/// Past spaces and comments from `i`.
fn trivia(text: &str, mut i: usize, syn: &SqlSyntax) -> usize {
    let b = text.as_bytes();
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        match skip_token(text, i, syn) {
            Some((end, Skipped::Comment)) => i = end,
            _ => return i,
        }
    }
}

/// The `)` matching the `(` at `open`, skipping strings, quoted names and
/// comments as `syn` reads them.
fn close_paren(text: &str, open: usize, syn: &SqlSyntax) -> Option<usize> {
    close_paren_from(text, open, 0, syn)
}

/// [`close_paren`] from `i`, `depth` parentheses already open.
fn close_paren_from(text: &str, mut i: usize, mut depth: usize, syn: &SqlSyntax) -> Option<usize> {
    let b = text.as_bytes();
    while i < b.len() {
        if let Some((end, _)) = skip_token(text, i, syn) {
            i = end;
            continue;
        }
        match b[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(r: &ConnectionRef) -> Result<ConnectorKind, String> {
        match r {
            ConnectionRef::Name(n) => match n.to_lowercase().as_str() {
                "pg" | "sales pg" => Ok(ConnectorKind::Postgres),
                "my" => Ok(ConnectorKind::Mysql),
                "ms" => Ok(ConnectorKind::Mssql),
                "ora" => Ok(ConnectorKind::Oracle),
                "mongo" => Ok(ConnectorKind::Mongodb),
                "graph" => Ok(ConnectorKind::Neo4j),
                _ => Err(format!("no connection named \"{n}\"")),
            },
            ConnectionRef::Id(i) if i == "c-42" => Ok(ConnectorKind::Snowflake),
            ConnectionRef::Id(i) => Err(format!("no connection with id {i}")),
        }
    }

    fn parse(text: &str) -> Result<Option<Combined>, CombinedError> {
        parse_combined(text, &mut kinds)
    }

    fn parts(text: &str) -> Vec<CombinedPart> {
        parse(text).unwrap().expect("a combined query").parts
    }

    #[test]
    fn two_parts_and_the_outer_query() {
        let q = "WITH\n  orders AS (@\"Sales PG\" SELECT id, total FROM orders WHERE day >= :since),\n  customers AS (@my SELECT id, name FROM customers)\nSELECT c.name, sum(o.total) FROM orders o JOIN customers c ON c.id = o.id GROUP BY 1";
        let p = parts(q);
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].name.as_str(), &p[0].connection, p[0].kind), ("orders", &ConnectionRef::Name("Sales PG".into()), ConnectorKind::Postgres));
        assert_eq!(p[0].sql, "SELECT id, total FROM orders WHERE day >= :since");
        assert_eq!(&q[p[0].sql_span.clone()], p[0].sql);
        assert_eq!((p[1].name.as_str(), p[1].kind, p[1].sql.as_str()), ("customers", ConnectorKind::Mysql, "SELECT id, name FROM customers"));
        let outer = Combined { parts: p }.outer_sql(q, |p| format!("(SELECT * FROM results.{})", p.name));
        assert_eq!(
            outer,
            "WITH\n  orders AS (SELECT * FROM results.orders),\n  customers AS (SELECT * FROM results.customers)\nSELECT c.name, sum(o.total) FROM orders o JOIN customers c ON c.id = o.id GROUP BY 1"
        );
    }

    #[test]
    fn a_closing_parenthesis_in_a_parts_own_strings_and_comments() {
        // Each dialect's own quoting: MySQL backticks, `\'` and #, SQL
        // Server brackets, Oracle q'[…]' (which DuckDB reads otherwise), and
        // Postgres' own (as DuckDB's).
        let cases = [
            ("pg", "select $$ ) $$, E'\\') ', \"a)\" from t -- )\n"),
            ("my", "select `a)`, 'it\\'s )' from t # )\n"),
            ("ms", "select [a)], N'x)' from t /* ) */"),
            ("ora", "select q'[ ) ]' from dual"),
            ("pg", "select (1 + (2)) as x from t"),
        ];
        for (conn, sql) in cases {
            let q = format!("with p as (@{conn} {sql}) select * from p");
            let p = parts(&q);
            assert_eq!(p[0].sql, sql.trim(), "{conn}: {sql}");
            assert_eq!(&q[p[0].body_span.clone()], format!("(@{conn} {sql})"));
        }
    }

    #[test]
    fn names_by_id_quotes_and_column_lists() {
        let q = r#"with recursive "My Orders"(a, b) as materialized (@id:c-42 select 1, 2), x as not materialized (@"Sales ""PG""" select 1) select * from "My Orders""#;
        let r = parse_combined(q, &mut |r| match r {
            ConnectionRef::Name(n) if n == "Sales \"PG\"" => Ok(ConnectorKind::Postgres),
            r => kinds(r),
        });
        let p = r.unwrap().unwrap().parts;
        assert_eq!((p[0].name.as_str(), &p[0].connection, p[0].kind), ("My Orders", &ConnectionRef::Id("c-42".into()), ConnectorKind::Snowflake));
        assert_eq!(p[1].connection, ConnectionRef::Name("Sales \"PG\"".into()));
    }

    #[test]
    fn ordinary_ctes_stay_in_the_outer_query() {
        let q = "-- report\nwith a as (select 1 as x), b as (@pg select x from t), c as (select * from a) select * from b join c using (x)";
        let p = parts(q);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].name, "b");
        let outer = Combined { parts: p }.outer_sql(q, |_| "(select 2 as x)".into());
        assert_eq!(outer, "-- report\nwith a as (select 1 as x), b as (select 2 as x), c as (select * from a) select * from b join c using (x)");
    }

    #[test]
    fn not_combined() {
        for q in [
            "select 1",
            "with a as (select 1) select * from a",
            "with a as (select '@pg' as x) select * from a",
            "select * from t where x = '@pg'",
            "with a as (select 1) select @pg",
            "WITH a AS (SELECT 1) SELECT @@ROWCOUNT",
            // `@` inside a CTE, not first: operators, variables, stages.
            "with a as (select * from t where tags @> '{x}') select * from a",
            "with a as (select @x := 1) select * from a",
            "with a as (select $1 from @stage) select * from a",
            "declare @x int; with a as (select @x as y) select * from a",
            "with",
            "",
        ] {
            assert_eq!(parse(q), Ok(None), "{q}");
        }
        assert!(!looks_combined("select * from @stage"));
        assert!(looks_combined("  /* x */ WITH a AS (@pg select 1) select 1"));
    }

    #[test]
    fn refusals_name_the_part() {
        let err = |q: &str| parse(q).unwrap_err();
        let e = err("with a as (@nope select 1) select * from a");
        assert_eq!(e.message, "Part `a`: no connection named \"nope\"");
        assert_eq!(e.position, 11);
        assert!(err("with a as (@mongo {\"find\": \"x\"}) select * from a").message.contains("MongoDB connections can't be parts yet"));
        assert!(err("with a as (@graph MATCH (n) RETURN n) select * from a").message.contains("can't be parts yet"));
        assert_eq!(err("with a as (@pg select (1) select * from a").message, "Part `a`: its closing parenthesis is missing");
        assert_eq!(err("with a as (@pg   ) select * from a").message, "Part `a` has no SQL after @pg");
        assert_eq!(err("with a as (@ select 1) select * from a").message, "Part `a`: name its connection after @ (@name, @\"a name\" or @id:…)");
        assert_eq!(err("with a as (@pg select 1)").message, "The query that reads the parts is missing after the WITH list");
        assert_eq!(err("with a as (@pg select 1), b as select 2 select 1").message, "Expected ( after `b AS`");
        assert_eq!(err("with a as (@id:zz select 1) select 1").message, "Part `a`: no connection with id zz");
    }

    /// What DuckDB allows around a CTE, and connection names that would
    /// mean something in the part's dialect.
    #[test]
    fn duckdb_ctes_and_names_read_as_duckdb() {
        assert_eq!(parts("with recursive as (@pg select 1) select * from recursive")[0].name, "recursive");
        assert_eq!(parts("with recursive r as (@pg select 1) select * from r")[0].name, "r");
        let p = parts("with a as (@pg select 1), b using key (x) as (select 1 as x) select 1");
        assert_eq!(p.len(), 1);
        assert_eq!(parts("with a as (@ID:c-42 select 1) select 1")[0].connection, ConnectionRef::Id("c-42".into()));
        // `#` is MySQL's comment, `'` a string start: the name ends before
        // them, and the part's own lexer reads them.
        assert_eq!(parts("with a as (@my#1\n select 1) select * from a")[0].sql, "#1\n select 1");
        assert_eq!(parse("with a as (@pg's select 1) select * from a").unwrap_err().message, "Part `a`: its closing parenthesis is missing");
        // A quoted name with a backslash, before a part MySQL reads.
        let q = r#"with a as (@"C:" select 'x)') select * from a"#;
        let r = parse_combined(q, &mut |_| Ok(ConnectorKind::Mysql)).unwrap().unwrap();
        assert_eq!(r.parts[0].sql, "select 'x)'");
        assert_eq!(parse("with a as (@pg select 1);").unwrap_err().message, "The query that reads the parts is missing after the WITH list");
    }

    #[test]
    fn spans_are_byte_offsets_with_non_ascii_text() {
        let q = "with ü as (@pg  select 'ä)ö' ) select * from ü";
        let p = parts(q);
        assert_eq!(p[0].name, "ü");
        assert_eq!(&q[p[0].sql_span.clone()], "select 'ä)ö'");
    }
}
