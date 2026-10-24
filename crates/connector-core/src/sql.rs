//! Client-side SQL helpers: splitting scripts into statements and classifying
//! statements (read vs. write) for safety checks.

use serde::Serialize;
use sqlparser::ast::{Query, SetExpr, Statement};
use sqlparser::dialect::{
    BigQueryDialect, DatabricksDialect, Dialect, DuckDbDialect, GenericDialect, MsSqlDialect, MySqlDialect, OracleDialect,
    PostgreSqlDialect, SQLiteDialect, SnowflakeDialect,
};
use sqlparser::parser::Parser;

use crate::traits::{BlockEnd, DollarQuotes, SourceTraits, SqlParser, SqlSyntax};
use crate::types::ConnectorKind;

pub fn dialect_for(kind: ConnectorKind) -> Box<dyn Dialect> {
    dialect_of(kind.traits().parser)
}

fn dialect_of(parser: SqlParser) -> Box<dyn Dialect> {
    match parser {
        SqlParser::Postgres => Box::new(PostgreSqlDialect {}),
        SqlParser::Mysql => Box::new(MySqlDialect {}),
        SqlParser::Sqlite => Box::new(SQLiteDialect {}),
        SqlParser::Mssql => Box::new(MsSqlDialect {}),
        SqlParser::Oracle => Box::new(OracleDialect {}),
        SqlParser::Snowflake => Box::new(SnowflakeDialect {}),
        SqlParser::Databricks => Box::new(DatabricksDialect {}),
        SqlParser::Bigquery => Box::new(BigQueryDialect {}),
        SqlParser::Duckdb => Box::new(DuckDbDialect {}),
        SqlParser::Generic => Box::new(GenericDialect {}),
    }
}

/// One statement in a script. `start`/`end` are byte offsets into the
/// original text (end exclusive, delimiter not included).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatementSpan {
    pub start: usize,
    pub end: usize,
    pub sql: String,
}

/// Lexical rules per dialect.
struct Lex {
    backslash_escapes: bool,
    backtick: bool,
    hash_comment: bool,
    slash_comment: bool,
    brackets: bool,
    /// Postgres `$tag$`; Snowflake `$$`.
    dollar: bool,
    triple_quotes: bool,
}

fn lex(kind: ConnectorKind) -> Lex {
    lex_of(&kind.traits().syntax)
}

fn lex_of(x: &SqlSyntax) -> Lex {
    Lex {
        backslash_escapes: x.backslash_escapes,
        backtick: x.backtick,
        hash_comment: x.hash_comment,
        slash_comment: x.slash_comment,
        brackets: x.brackets,
        dollar: x.dollar != DollarQuotes::None,
        triple_quotes: x.triple_quotes,
    }
}

/// How a procedural block that contains `;` ends.
#[derive(PartialEq)]
enum Block {
    None,
    /// Oracle PL/SQL: a line containing only `/`.
    SlashLine,
    /// SQL Server: a `GO` line (or end of script).
    GoLine,
    /// SQLite trigger / BigQuery procedure: `;` after the word END.
    EndWord,
}

fn block_kind(x: &SqlSyntax, seg: &str) -> Block {
    let mut w = words(strip_leading_comments(seg)).map(|x| x.to_ascii_uppercase());
    let first = w.next().unwrap_or_default();
    let rest: Vec<String> = w.take(12).collect();
    let after_create = || -> Option<&str> {
        let mut it = rest.iter().map(String::as_str).peekable();
        if it.peek() == Some(&"OR") {
            it.next();
            it.next(); // REPLACE / ALTER
        }
        // MySQL: CREATE DEFINER = user@host [SQL SECURITY …] PROCEDURE.
        if it.peek() == Some(&"DEFINER") {
            return it.find(|o| x.block_objects.contains(o));
        }
        while matches!(
            it.peek(),
            Some(&"EDITIONABLE") | Some(&"NONEDITIONABLE") | Some(&"TEMP") | Some(&"TEMPORARY")
        ) {
            it.next();
        }
        it.next()
    };
    let block = match x.block_end {
        BlockEnd::None => return Block::None,
        BlockEnd::SlashLine => Block::SlashLine,
        BlockEnd::GoLine => Block::GoLine,
        BlockEnd::EndWord => Block::EndWord,
    };
    if x.block_starts.contains(&first.as_str()) {
        return block;
    }
    if (first == "CREATE" || (x.block_on_alter && first == "ALTER")) && after_create().is_some_and(|o| x.block_objects.contains(&o)) {
        return block;
    }
    // Oracle 12c: WITH FUNCTION f … END; SELECT … (ends with a `/` line).
    if first == "WITH" && block == Block::SlashLine && matches!(rest.first().map(String::as_str), Some("FUNCTION" | "PROCEDURE")) {
        return block;
    }
    Block::None
}

/// Words that keep `BEGIN` from opening a block (`BEGIN TRANSACTION;`).
const BEGIN_NOT_BLOCK: &[&str] = &["TRANSACTION", "TRAN", "WORK", "DEFERRED", "IMMEDIATE", "EXCLUSIVE", "DISTRIBUTED", "NAME"];
/// `END IF` / `END LOOP` … close constructs that are not counted as opened.
const END_OF_UNCOUNTED: &[&str] = &["IF", "LOOP", "WHILE", "REPEAT", "FOR"];

/// The next word after byte `i` (skipping spaces), upper-cased; empty when
/// something else (`;`, `(`) comes first.
fn next_word(text: &str, i: usize) -> String {
    let rest = text[i..].trim_start();
    rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).next().unwrap_or("").to_ascii_uppercase()
}

/// Whether a statement can stand on its own (for blank-line separation).
fn parses_alone(sql: &str, parser: SqlParser) -> bool {
    Parser::parse_sql(dialect_of(parser).as_ref(), sql).is_ok_and(|s| s.len() == 1)
}

/// First words of a statement, for blank-line separation.
const STATEMENT_STARTS: &[&str] = &[
    "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "ALTER", "DROP", "TRUNCATE", "EXEC", "EXECUTE", "CALL",
    "DECLARE", "SET", "USE", "GRANT", "REVOKE", "BEGIN", "COMMIT", "ROLLBACK", "EXPLAIN", "SHOW", "DESCRIBE", "VALUES",
];

fn is_go_line(line: &str) -> bool {
    let mut it = line.split_whitespace();
    it.next().is_some_and(|w| w.eq_ignore_ascii_case("go"))
        && it
            .next()
            .is_none_or(|n| n.chars().all(|c| c.is_ascii_digit()))
        && it.next().is_none()
}

/// Split a script into statements, respecting quotes, comments and each
/// dialect's procedural-block conventions: Postgres/Snowflake dollar quoting,
/// MySQL `DELIMITER`, Oracle `/` lines, SQL Server `GO`, SQLite triggers.
pub fn split_statements(text: &str, kind: ConnectorKind) -> Vec<StatementSpan> {
    split_with(text, kind.traits())
}

/// [`split_statements`] for a source's traits (testable without a kind).
pub fn split_with(text: &str, t: &SourceTraits) -> Vec<StatementSpan> {
    if !t.language.splits_on_semicolon() {
        return crate::language::split_whole(text);
    }
    let syntax = &t.syntax;
    let b = text.as_bytes();
    let n = b.len();
    let lx = lex_of(syntax);

    let mut out = Vec::new();
    let mut delimiter: Vec<u8> = b";".to_vec();
    let mut seg_start = 0usize;
    let mut has_code = false;
    let mut i = 0usize;
    // Open BEGIN/CASE … END blocks in the current statement (EndWord dialects).
    let mut depth = 0i32;
    // A blank line ends a statement written without `;` when the next line
    // starts another one. Not in T-SQL batches with variables: each
    // statement runs on its own, so a DECLARE must stay with its uses.
    let blank_lines_split = !(syntax.block_end == BlockEnd::GoLine && text.to_ascii_lowercase().contains("declare"));

    let push = |out: &mut Vec<StatementSpan>, from: usize, to: usize, has_code: bool| {
        if !has_code {
            return;
        }
        let raw = &text[from..to];
        let lead = raw.len() - raw.trim_start().len();
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            out.push(StatementSpan {
                start: from + lead,
                end: from + lead + trimmed.len(),
                sql: trimmed.to_string(),
            });
        }
    };

    while i < n {
        // Line-level directives.
        if i == 0 || b[i - 1] == b'\n' {
            let line_end = text[i..].find('\n').map(|p| i + p).unwrap_or(n);
            let line = text[i..line_end].trim();
            let directive = match syntax.block_end {
                _ if syntax.delimiter_directive
                    && line.len() > 10
                        && line.is_char_boundary(10)
                        && line[..10].eq_ignore_ascii_case("delimiter ") =>
                {
                    let d = line[10..].trim();
                    if !d.is_empty() {
                        delimiter = d.as_bytes().to_vec();
                    }
                    true
                }
                BlockEnd::SlashLine => line == "/",
                BlockEnd::GoLine => is_go_line(line),
                _ => false,
            };
            if directive {
                push(&mut out, seg_start, i, has_code);
                i = (line_end + 1).min(n);
                seg_start = i;
                has_code = false;
                depth = 0;
                continue;
            }
            if line.is_empty() && has_code && blank_lines_split && delimiter == b";" && depth == 0 {
                let next = text[line_end..].trim_start();
                let first = next.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).next().unwrap_or("").to_ascii_uppercase();
                let seg = &text[seg_start..i];
                if STATEMENT_STARTS.contains(&first.as_str())
                    && block_kind(syntax, seg) == Block::None
                    && !matches!(leading_keyword(seg).as_str(), "DECLARE" | "SET")
                    && parses_alone(seg.trim(), t.parser)
                {
                    push(&mut out, seg_start, i, has_code);
                    seg_start = i;
                    has_code = false;
                }
            }
        }

        if b[i..].starts_with(&delimiter) {
            let seg = &text[seg_start..i];
            let keep = delimiter == b";"
                && match block_kind(syntax, seg) {
                    Block::None => false,
                    Block::SlashLine | Block::GoLine => true,
                    Block::EndWord => depth > 0,
                };
            if keep {
                i += 1;
                continue;
            }
            push(&mut out, seg_start, i, has_code);
            i += delimiter.len();
            seg_start = i;
            has_code = false;
            depth = 0;
            continue;
        }

        let c = b[i];
        match c {
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                i = skip_line(b, i);
                continue;
            }
            b'#' if lx.hash_comment => {
                i = skip_line(b, i);
                continue;
            }
            b'/' if lx.slash_comment && i + 1 < n && b[i + 1] == b'/' => {
                i = skip_line(b, i);
                continue;
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i = match text[i + 2..].find("*/") {
                    Some(p) => i + 2 + p + 2,
                    None => n,
                };
                continue;
            }
            _ => {}
        }

        if !c.is_ascii_whitespace() {
            has_code = true;
        }

        let triple: &[u8] = if c == b'"' { b"\"\"\"" } else { b"'''" };
        match c {
            b'\'' | b'"' if lx.triple_quotes && b[i..].starts_with(triple) => {
                let q = &text[i..i + 3];
                i = match text[i + 3..].find(q) {
                    Some(p) => i + 3 + p + 3,
                    None => n,
                };
            }
            b'\'' => i = skip_quoted(b, i, b'\'', lx.backslash_escapes),
            b'"' => {
                i = skip_quoted(
                    b,
                    i,
                    b'"',
                    syntax.backslash_in_double_quotes,
                )
            }
            b'`' if lx.backtick => i = skip_quoted(b, i, b'`', false),
            b'[' if lx.brackets => {
                i = match text[i..].find(']') {
                    Some(p) => i + p + 1,
                    None => n,
                }
            }
            b'$' if lx.dollar => match dollar_tag(text, i) {
                Some(tag) if syntax.dollar == DollarQuotes::Tagged || tag == "$$" => {
                    let body = i + tag.len();
                    i = match text[body..].find(tag) {
                        Some(p) => body + p + tag.len(),
                        None => n,
                    };
                }
                _ => i += 1,
            },
            c if (c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'$' || b[i - 1] == b'.')) => {
                let mut j = i + 1;
                while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                if syntax.block_end == BlockEnd::EndWord {
                    let w = &text[i..j];
                    if w.eq_ignore_ascii_case("BEGIN") {
                        let next = next_word(text, j);
                        let opens = !BEGIN_NOT_BLOCK.contains(&next.as_str()) && !text[j..].trim_start().starts_with(';');
                        depth += opens as i32;
                    } else if w.eq_ignore_ascii_case("CASE") {
                        depth += 1;
                    } else if w.eq_ignore_ascii_case("END") && !END_OF_UNCOUNTED.contains(&next_word(text, j).as_str()) {
                        depth = (depth - 1).max(0);
                    }
                }
                i = j;
            }
            _ => i += 1,
        }
    }
    push(&mut out, seg_start, n, has_code);
    out
}

fn skip_line(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    i
}

/// Skip a quoted literal/identifier starting at `i`. Doubled quotes are
/// escapes; backslash escapes apply for MySQL string literals.
fn skip_quoted(b: &[u8], i: usize, q: u8, backslash: bool) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if backslash && b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == q {
            if j + 1 < b.len() && b[j + 1] == q {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// Returns the `$tag$` opener at `i` if present (not a `$1` parameter).
fn dollar_tag(text: &str, i: usize) -> Option<&str> {
    let b = text.as_bytes();
    if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return None;
    }
    let mut j = i + 1;
    if j < b.len() && b[j].is_ascii_digit() {
        return None;
    }
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    (j < b.len() && b[j] == b'$').then(|| &text[i..=j])
}

pub(crate) fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

fn strip_leading_comments(mut s: &str) -> &str {
    loop {
        s = s.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|x| x.1).unwrap_or("");
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|x| x.1).unwrap_or("");
        } else {
            return s;
        }
    }
}

/// First keyword of a statement, upper-cased (skips comments and parens).
pub fn leading_keyword(sql: &str) -> String {
    words(strip_leading_comments(sql))
        .next()
        .unwrap_or("")
        .to_ascii_uppercase()
}

/// Pick the statement for "run statement at cursor". `cursor` is a byte
/// offset. If the cursor sits between statements, the preceding one wins.
pub fn statement_at(spans: &[StatementSpan], cursor: usize) -> Option<&StatementSpan> {
    spans
        .iter()
        .find(|s| cursor >= s.start && cursor <= s.end)
        .or_else(|| spans.iter().rev().find(|s| s.end < cursor))
        .or_else(|| spans.first())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    Read,
    Session,
    Transaction,
    Dml,
    Ddl,
    Other,
    Unknown,
}

impl StatementKind {
    pub fn is_read(self) -> bool {
        self == StatementKind::Read
    }
    /// Statements that change data or schema.
    pub fn is_write(self) -> bool {
        matches!(self, StatementKind::Dml | StatementKind::Ddl)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Classification {
    pub kind: StatementKind,
    /// `UPDATE`/`DELETE` without a `WHERE` clause.
    pub missing_where: bool,
    pub keyword: String,
}

/// Classify one statement. Uses the SQL parser when possible and falls back
/// to keyword heuristics (conservatively) when parsing fails.
pub fn classify(sql: &str, kind: ConnectorKind) -> Classification {
    let language = kind.traits().language;
    if !language.is_sql() {
        return crate::language::classify(sql, language);
    }
    let keyword = leading_keyword(sql);
    let dialect = dialect_for(kind);
    match Parser::parse_sql(dialect.as_ref(), sql) {
        Ok(stmts) if !stmts.is_empty() => {
            let mut out = Classification {
                kind: StatementKind::Read,
                missing_where: false,
                keyword: keyword.clone(),
            };
            for s in &stmts {
                let (k, mw) = classify_ast(s, &keyword);
                out.kind = out.kind.max(k);
                out.missing_where |= mw;
            }
            out
        }
        _ => {
            let mut k = keyword_kind(&keyword);
            if k == StatementKind::Read && keyword == "WITH" && contains_dml_word(sql) {
                k = StatementKind::Dml;
            }
            if k == StatementKind::Read {
                // Unparseable but looks like a read; don't claim certainty.
                k = if keyword == "SELECT" && !contains_dml_word(sql) {
                    StatementKind::Read
                } else {
                    StatementKind::Unknown
                };
            }
            Classification {
                kind: k,
                missing_where: false,
                keyword,
            }
        }
    }
}

fn classify_ast(s: &Statement, keyword: &str) -> (StatementKind, bool) {
    use StatementKind as K;
    match s {
        Statement::Query(q) => (query_kind(q), false),
        Statement::Insert(_) | Statement::Merge(_) => (K::Dml, false),
        Statement::Update(u) => (K::Dml, u.selection.is_none()),
        Statement::Delete(d) => (K::Dml, d.selection.is_none()),
        Statement::Explain {
            analyze, statement, ..
        } => {
            if *analyze {
                classify_ast(statement, &leading_keyword(&statement.to_string()))
            } else {
                (K::Read, false)
            }
        }
        Statement::ExplainTable { .. } => (K::Read, false),
        _ => (keyword_kind(keyword), false),
    }
}

fn query_kind(q: &Query) -> StatementKind {
    let mut k = set_expr_kind(&q.body);
    if let Some(with) = &q.with {
        for cte in &with.cte_tables {
            k = k.max(query_kind(&cte.query));
        }
    }
    k
}

fn set_expr_kind(e: &SetExpr) -> StatementKind {
    match e {
        SetExpr::Select(s) if s.into.is_some() => StatementKind::Ddl,
        SetExpr::Select(_) | SetExpr::Values(_) | SetExpr::Table(_) => StatementKind::Read,
        SetExpr::Query(q) => query_kind(q),
        SetExpr::SetOperation { left, right, .. } => set_expr_kind(left).max(set_expr_kind(right)),
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_) => {
            StatementKind::Dml
        }
        #[allow(unreachable_patterns)]
        _ => StatementKind::Unknown,
    }
}

pub(crate) fn keyword_kind(kw: &str) -> StatementKind {
    use StatementKind as K;
    match kw {
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "VALUES" | "TABLE" => {
            K::Read
        }
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" | "UPSERT" | "COPY" | "LOAD"
        | "CALL" | "EXEC" | "EXECUTE" | "DO" | "HANDLER" => K::Dml,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "COMMENT" | "GRANT" | "REVOKE" => {
            K::Ddl
        }
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "END" | "ABORT"
        | "XA" => K::Transaction,
        "SET" | "USE" | "RESET" | "ATTACH" | "DETACH" | "PRAGMA" | "DISCARD" | "LISTEN"
        | "UNLISTEN" => K::Session,
        "" => K::Unknown,
        _ => K::Other,
    }
}

fn contains_dml_word(sql: &str) -> bool {
    words(sql).any(|w| {
        [
            "INSERT", "UPDATE", "DELETE", "MERGE", "INTO", "DROP", "ALTER", "CREATE", "TRUNCATE",
        ]
        .iter()
        .any(|k| w.eq_ignore_ascii_case(k))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectorKind::*;

    fn sqls(text: &str, k: ConnectorKind) -> Vec<String> {
        split_statements(text, k)
            .into_iter()
            .map(|s| s.sql)
            .collect()
    }

    #[test]
    fn split_basic_and_quotes() {
        let t = "select 1; select ';' as x -- c;\n; select \"a;b\" from t;";
        assert_eq!(
            sqls(t, Postgres),
            vec!["select 1", "select ';' as x -- c;", "select \"a;b\" from t"]
        );
    }

    #[test]
    fn split_skips_comment_only_segments() {
        let t = "-- just a comment\n;\n/* x; */ select 2;";
        assert_eq!(sqls(t, Postgres), vec!["/* x; */ select 2"]);
    }

    #[test]
    fn split_pg_dollar_quotes() {
        let t = "create function f() returns int as $$ begin; return 1; end $$ language plpgsql; select $1";
        let v = sqls(t, Postgres);
        assert_eq!(v.len(), 2);
        assert!(v[0].ends_with("plpgsql"));
        assert_eq!(v[1], "select $1");
    }

    #[test]
    fn split_mysql_delimiter_and_backslash() {
        let t = "select 'it\\'s;';\nDELIMITER $$\ncreate procedure p() begin select 1; end$$\nDELIMITER ;\nselect 3;";
        let v = sqls(t, Mysql);
        assert_eq!(v.len(), 3, "{v:?}");
        assert!(v[1].starts_with("create procedure") && v[1].ends_with("end"));
        assert_eq!(v[2], "select 3");
    }

    #[test]
    fn split_sqlite_trigger() {
        let t = "create trigger tr after insert on t begin update t set a=1; delete from u; end; select 1;";
        let v = sqls(t, Sqlite);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].ends_with("end"));
    }

    #[test]
    fn split_oracle_plsql() {
        let t = "select 1 from dual;\ncreate or replace procedure p as\nbegin\n  null;\nend;\n/\nbegin\n  p;\nend;\n/\nselect 2 from dual";
        let v = sqls(t, Oracle);
        assert_eq!(v.len(), 4, "{v:?}");
        assert!(v[1].ends_with("end;"));
        assert_eq!(v[2], "begin\n  p;\nend;");
    }

    #[test]
    fn split_mssql_go_batches() {
        let t = "select 1; select [a;b] from t\nGO\ncreate procedure p as begin select 1; select 2; end\ngo\nselect 3";
        let v = sqls(t, Mssql);
        assert_eq!(v.len(), 4, "{v:?}");
        assert!(v[2].starts_with("create procedure") && v[2].ends_with("end"));
    }

    #[test]
    fn split_cloud_dialects() {
        assert_eq!(
            sqls("select `a;b` from t; select 'x\\';y'", Databricks).len(),
            2
        );
        assert_eq!(sqls("select '''a;b''' # c;\n; select 2", Bigquery).len(), 2);
        let v = sqls(
            "create procedure p() returns int language sql as $$ begin return 1; end $$; // c;\nselect 1",
            Snowflake,
        );
        assert_eq!(v.len(), 2, "{v:?}");
    }

    #[test]
    fn split_procedure_bodies_and_nested_ends() {
        let v = |k: ConnectorKind, t: &str| sqls(t, k);
        // MySQL bodies without DELIMITER, DEFINER, CASE … END inside.
        assert_eq!(v(Mysql, "create procedure p() begin select 1; select 2; end;\nselect 3;"), vec!["create procedure p() begin select 1; select 2; end", "select 3"]);
        assert_eq!(v(Mysql, "create definer=`root`@`%` procedure p() begin select 1; end;\nselect 3;").len(), 2);
        assert_eq!(
            v(Mysql, "create procedure p() begin declare x int; set x = case when 1 then 2 else 3 end; if x > 1 then select 1; end if; end;\nselect 3;").len(),
            2
        );
        assert_eq!(v(Mysql, "create function f() returns int deterministic return 1;\nselect f();").len(), 2, "single-statement body");
        // Repeated DELIMITER switches.
        assert_eq!(
            v(Mysql, "delimiter //\ncreate procedure a() begin select 1; end //\ndelimiter ;\nselect 1;\ndelimiter $$\ncreate procedure b() begin select 2; end$$\ndelimiter ;\nselect 2;"),
            vec!["create procedure a() begin select 1; end", "select 1", "create procedure b() begin select 2; end", "select 2"]
        );
        // SQLite trigger with CASE; BEGIN TRANSACTION is not a block.
        assert_eq!(v(Sqlite, "create trigger t after insert on x begin update y set a = case when new.a > 0 then 1 else 0 end; end;\nselect 1;").len(), 2);
        assert_eq!(v(Sqlite, "begin transaction;\ninsert into t values (1);\ncommit;").len(), 3);
        assert_eq!(v(Bigquery, "create procedure d.p() begin select case when true then 1 end; select 2; end;\nselect 3;").len(), 2);
        assert_eq!(v(Postgres, "create function f() returns int language sql begin atomic select 1; select 2; end;\nselect 3;").len(), 2);
        assert_eq!(v(Postgres, "begin;\nselect 1;\ncommit;").len(), 3);
        // Oracle WITH FUNCTION ends at `/`; backslashes are plain text.
        let o = v(Oracle, "with function f(x number) return number is begin return x * 2; end;\nselect f(1) from dual\n/\nselect 2 from dual;");
        assert_eq!(o.len(), 2, "{o:?}");
        assert!(o[0].ends_with("select f(1) from dual"));
        assert_eq!(v(Oracle, "select 'C:\\' from dual; select 2 from dual;").len(), 2);
    }

    #[test]
    fn split_statements_without_semicolons() {
        assert_eq!(sqls("select 1\n\nselect 2\n\nupdate t set a = 1\n\nwhere b = 2", Postgres), vec!["select 1", "select 2", "update t set a = 1\n\nwhere b = 2"]);
        assert_eq!(sqls("select 1\n\nselect 2", Mssql), vec!["select 1", "select 2"]);
        // A blank line inside a statement keeps it whole.
        assert_eq!(sqls("insert into t\n\nselect * from x;\nwith a as (select 1)\n\nselect * from a;", Postgres).len(), 2);
        assert_eq!(sqls("select a\n\nfrom t", Postgres).len(), 1);
        // T-SQL variables stay with their batch.
        assert_eq!(sqls("declare @n int = 5\n\nselect @n", Mssql).len(), 1);
    }

    #[test]
    fn cursor_selection() {
        let t = "select 1;\n\nselect 2;";
        let spans = split_statements(t, Postgres);
        assert_eq!(statement_at(&spans, 0).unwrap().sql, "select 1");
        assert_eq!(statement_at(&spans, 10).unwrap().sql, "select 1");
        assert_eq!(statement_at(&spans, 13).unwrap().sql, "select 2");
    }

    #[test]
    fn classify_statements() {
        let c = |s: &str| classify(s, Postgres).kind;
        assert_eq!(c("select * from t"), StatementKind::Read);
        assert_eq!(
            c("with x as (select 1) select * from x"),
            StatementKind::Read
        );
        assert_eq!(
            c("with d as (delete from t returning *) select * from d"),
            StatementKind::Dml
        );
        assert_eq!(c("insert into t values (1)"), StatementKind::Dml);
        assert_eq!(c("drop table t"), StatementKind::Ddl);
        assert_eq!(c("select * into t2 from t"), StatementKind::Ddl);
        assert_eq!(c("begin"), StatementKind::Transaction);
        assert_eq!(c("explain select 1"), StatementKind::Read);
        assert_eq!(c("explain analyze delete from t"), StatementKind::Dml);
        assert!(classify("delete from t", Postgres).missing_where);
        assert!(!classify("update t set a=1 where id=2", Postgres).missing_where);
        assert_eq!(classify("show tables", Mysql).kind, StatementKind::Read);
        assert_eq!(c("totally not sql ;;"), StatementKind::Other);
    }
}

// ------------------------------------------------------------------ parameters

/// A `:name` parameter in a script (byte offsets of `:name`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParamSpan {
    pub name: String,
    pub start: usize,
    pub end: usize,
}

/// `:name` parameters outside strings, quoted identifiers and comments.
/// Not parameters: `::type` casts, `col:field` / `$1:field` paths (a name,
/// digit, quote or bracket right before the colon), `:=` assignments, and
/// Oracle trigger `:new` / `:old`.
pub fn find_parameters(text: &str, kind: ConnectorKind) -> Vec<ParamSpan> {
    if !kind.traits().syntax.colon_params {
        return vec![];
    }
    let b = text.as_bytes();
    let n = b.len();
    let lx = lex(kind);
    let mut out = Vec::new();
    let mut i = 0usize;
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < n {
        let c = b[i];
        match c {
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
            b'\'' => i = skip_quoted(b, i, b'\'', lx.backslash_escapes),
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
                    out.push(ParamSpan { name: name.to_string(), start: i, end: j });
                }
                i = j;
            }
            _ => i += 1,
        }
    }
    out
}

/// A value typed for a parameter.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
pub struct ParamValue {
    pub value: String,
    /// Insert as typed (an SQL expression such as `current_date`), no quoting.
    #[serde(default)]
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

/// One complete `'…'` literal (optionally `N'…'` / `E'…'`).
fn is_string_literal(t: &str, kind: ConnectorKind) -> bool {
    let b = t.as_bytes();
    let start = match b.first() {
        Some(b'\'') => 0,
        Some(b'N' | b'n' | b'E' | b'e') if b.get(1) == Some(&b'\'') => 1,
        _ => return false,
    };
    b.len() >= start + 2 && skip_quoted(b, start, b'\'', lex(kind).backslash_escapes) == b.len() && b[b.len() - 1] == b'\''
}

/// `'text'` with the dialect's escaping (doubled quotes; backslashes where
/// the dialect treats them as escapes).
pub fn quote_string(t: &str, kind: ConnectorKind) -> String {
    if lex(kind).backslash_escapes {
        format!("'{}'", t.replace('\\', "\\\\").replace('\'', "\\'"))
    } else {
        format!("'{}'", t.replace('\'', "''"))
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
        match values.get(&s.name).and_then(|v| render_param(v, kind)) {
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
    use std::collections::HashMap;
    use ConnectorKind as K;

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
