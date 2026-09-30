//! Client-side SQL helpers: splitting scripts into statements and classifying
//! statements (read vs. write) for safety checks.

use serde::Serialize;
use sqlparser::ast::{Query, SetExpr, Statement};
use sqlparser::dialect::{
    BigQueryDialect, DatabricksDialect, Dialect, DuckDbDialect, MsSqlDialect, MySqlDialect, OracleDialect,
    PostgreSqlDialect, SQLiteDialect, SnowflakeDialect,
};
use sqlparser::parser::Parser;

use crate::types::ConnectorKind;

pub fn dialect_for(kind: ConnectorKind) -> Box<dyn Dialect> {
    match kind {
        ConnectorKind::Postgres => Box::new(PostgreSqlDialect {}),
        ConnectorKind::Mysql => Box::new(MySqlDialect {}),
        ConnectorKind::Sqlite => Box::new(SQLiteDialect {}),
        ConnectorKind::Mssql => Box::new(MsSqlDialect {}),
        ConnectorKind::Oracle => Box::new(OracleDialect {}),
        ConnectorKind::Snowflake => Box::new(SnowflakeDialect {}),
        ConnectorKind::Databricks => Box::new(DatabricksDialect {}),
        ConnectorKind::Bigquery => Box::new(BigQueryDialect {}),
        ConnectorKind::Duckdb => Box::new(DuckDbDialect {}),
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
    use ConnectorKind as K;
    Lex {
        backslash_escapes: matches!(kind, K::Mysql | K::Bigquery | K::Databricks | K::Snowflake),
        backtick: matches!(kind, K::Mysql | K::Sqlite | K::Databricks | K::Bigquery),
        hash_comment: matches!(kind, K::Mysql | K::Bigquery),
        slash_comment: kind == K::Snowflake,
        brackets: matches!(kind, K::Sqlite | K::Mssql),
        dollar: matches!(kind, K::Postgres | K::Snowflake | K::Duckdb),
        triple_quotes: kind == K::Bigquery,
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

fn block_kind(kind: ConnectorKind, seg: &str) -> Block {
    let mut w = words(strip_leading_comments(seg)).map(|x| x.to_ascii_uppercase());
    let first = w.next().unwrap_or_default();
    let rest: Vec<String> = w.take(6).collect();
    let after_create = || -> Option<&str> {
        let mut it = rest.iter().map(String::as_str).peekable();
        if it.peek() == Some(&"OR") {
            it.next();
            it.next(); // REPLACE / ALTER
        }
        while matches!(
            it.peek(),
            Some(&"EDITIONABLE") | Some(&"NONEDITIONABLE") | Some(&"TEMP") | Some(&"TEMPORARY")
        ) {
            it.next();
        }
        it.next()
    };
    match kind {
        ConnectorKind::Oracle => {
            if first == "DECLARE" || first == "BEGIN" {
                return Block::SlashLine;
            }
            if first == "CREATE"
                && after_create().is_some_and(|o| {
                    matches!(o, "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE")
                })
            {
                return Block::SlashLine;
            }
            Block::None
        }
        ConnectorKind::Mssql => {
            if (first == "CREATE" || first == "ALTER")
                && after_create().is_some_and(|o| {
                    matches!(o, "PROCEDURE" | "PROC" | "FUNCTION" | "TRIGGER" | "VIEW")
                })
            {
                return Block::GoLine;
            }
            Block::None
        }
        ConnectorKind::Sqlite if first == "CREATE" && after_create() == Some("TRIGGER") => {
            Block::EndWord
        }
        ConnectorKind::Bigquery if first == "CREATE" && after_create() == Some("PROCEDURE") => {
            Block::EndWord
        }
        _ => Block::None,
    }
}

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
    let b = text.as_bytes();
    let n = b.len();
    let lx = lex(kind);

    let mut out = Vec::new();
    let mut delimiter: Vec<u8> = b";".to_vec();
    let mut seg_start = 0usize;
    let mut has_code = false;
    let mut i = 0usize;

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
            let directive = match kind {
                ConnectorKind::Mysql
                    if line.len() > 10
                        && line.is_char_boundary(10)
                        && line[..10].eq_ignore_ascii_case("delimiter ") =>
                {
                    let d = line[10..].trim();
                    if !d.is_empty() {
                        delimiter = d.as_bytes().to_vec();
                    }
                    true
                }
                ConnectorKind::Oracle => line == "/",
                ConnectorKind::Mssql => is_go_line(line),
                _ => false,
            };
            if directive {
                push(&mut out, seg_start, i, has_code);
                i = (line_end + 1).min(n);
                seg_start = i;
                has_code = false;
                continue;
            }
        }

        if b[i..].starts_with(&delimiter) {
            let seg = &text[seg_start..i];
            let keep = delimiter == b";"
                && match block_kind(kind, seg) {
                    Block::None => false,
                    Block::SlashLine | Block::GoLine => true,
                    Block::EndWord => !last_word_is_end(seg),
                };
            if keep {
                i += 1;
                continue;
            }
            push(&mut out, seg_start, i, has_code);
            i += delimiter.len();
            seg_start = i;
            has_code = false;
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
                    lx.backslash_escapes && kind != ConnectorKind::Snowflake,
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
                Some(tag) if matches!(kind, ConnectorKind::Postgres | ConnectorKind::Duckdb) || tag == "$$" => {
                    let body = i + tag.len();
                    i = match text[body..].find(tag) {
                        Some(p) => body + p + tag.len(),
                        None => n,
                    };
                }
                _ => i += 1,
            },
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

fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

fn last_word_is_end(s: &str) -> bool {
    words(s)
        .last()
        .is_some_and(|w| w.eq_ignore_ascii_case("end"))
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

fn keyword_kind(kw: &str) -> StatementKind {
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
