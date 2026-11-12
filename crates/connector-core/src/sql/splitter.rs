use serde::Serialize;
use sqlparser::parser::Parser;

use super::dialect_of;
use crate::traits::{BlockEnd, DollarQuotes, SourceTraits, SqlParser, SqlSyntax};
use crate::types::ConnectorKind;

/// One statement in a script. `start`/`end` are byte offsets into the
/// original text (end exclusive, delimiter not included).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatementSpan {
    pub start: usize,
    pub end: usize,
    pub sql: String,
}

/// What [`skip_token`] skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skipped {
    Comment,
    /// A `'…'` literal (`start` is its opening quote).
    SingleQuoted {
        start: usize,
    },
    /// Any other quoted text: `"…"`, `` `…` ``, `[…]`, `$tag$…$tag$`,
    /// `'''…'''`, `E'…'`, `q'[…]'`.
    Quoted,
}

/// The one reading of a dialect's comments and quoted text, shared by the
/// splitter and the parameter finder so they can't disagree about where a
/// string ends: when one starts at byte `i`, where it ends and what it was.
/// `None` when `i` starts neither.
pub(crate) fn skip_token(text: &str, i: usize, syntax: &SqlSyntax) -> Option<(usize, Skipped)> {
    let b = text.as_bytes();
    let n = b.len();
    let c = *b.get(i)?;
    let next = b.get(i + 1).copied();
    let ident = |k: usize| k > 0 && (b[k - 1].is_ascii_alphanumeric() || b[k - 1] == b'_' || b[k - 1] == b'$' || b[k - 1] == b'.');
    let end = match c {
        b'-' if next == Some(b'-') => return Some((skip_line(b, i), Skipped::Comment)),
        b'#' if syntax.hash_comment => return Some((skip_line(b, i), Skipped::Comment)),
        b'/' if syntax.slash_comment && next == Some(b'/') => return Some((skip_line(b, i), Skipped::Comment)),
        b'/' if next == Some(b'*') => return Some((text[i + 2..].find("*/").map_or(n, |p| i + 2 + p + 2), Skipped::Comment)),
        b'\'' | b'"' if syntax.triple_quotes && b[i..].starts_with(if c == b'"' { b"\"\"\"" } else { b"'''" }) => {
            let q = &text[i..i + 3];
            text[i + 3..].find(q).map_or(n, |p| i + 3 + p + 3)
        }
        b'\'' => return Some((skip_quoted(b, i, b'\'', syntax.backslash_escapes), Skipped::SingleQuoted { start: i })),
        b'"' => skip_quoted(b, i, b'"', syntax.backslash_in_double_quotes),
        b'`' if syntax.backtick => skip_quoted(b, i, b'`', false),
        b'[' if syntax.brackets => text[i..].find(']').map_or(n, |p| i + p + 1),
        b'$' if syntax.dollar != DollarQuotes::None => match dollar_tag(text, i) {
            Some(tag) if syntax.dollar == DollarQuotes::Tagged || tag == "$$" => {
                let body = i + tag.len();
                text[body..].find(tag).map_or(n, |p| body + p + tag.len())
            }
            _ => return None,
        },
        // Postgres/DuckDB E'…': backslashes escape inside.
        b'E' | b'e' if syntax.escape_strings && next == Some(b'\'') && !ident(i) => skip_quoted(b, i + 1, b'\'', true),
        // Oracle q'[…]' / nq'{…}': up to the closing delimiter and quote.
        b'Q' | b'q' | b'N' | b'n' if syntax.q_quotes && !ident(i) => {
            let q = if matches!(c, b'N' | b'n') && matches!(next, Some(b'q' | b'Q')) { i + 1 } else { i };
            if !matches!(b[q], b'q' | b'Q') || b.get(q + 1) != Some(&b'\'') || q + 2 >= n {
                return None;
            }
            let close = match b[q + 2] {
                b'[' => b']',
                b'{' => b'}',
                b'(' => b')',
                b'<' => b'>',
                c => c,
            };
            (q + 3..n.saturating_sub(1)).find(|&k| b[k] == close && b[k + 1] == b'\'').map_or(n, |k| k + 2)
        }
        _ => return None,
    };
    Some((end, Skipped::Quoted))
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
        while matches!(it.peek(), Some(&"EDITIONABLE") | Some(&"NONEDITIONABLE") | Some(&"TEMP") | Some(&"TEMPORARY")) {
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
    "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "ALTER", "DROP", "TRUNCATE", "EXEC", "EXECUTE", "CALL", "DECLARE", "SET", "USE",
    "GRANT", "REVOKE", "BEGIN", "COMMIT", "ROLLBACK", "EXPLAIN", "SHOW", "DESCRIBE", "VALUES",
];

fn is_go_line(line: &str) -> bool {
    let mut it = line.split_whitespace();
    it.next().is_some_and(|w| w.eq_ignore_ascii_case("go")) && it.next().is_none_or(|n| n.chars().all(|c| c.is_ascii_digit())) && it.next().is_none()
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

    let mut out = Vec::new();
    let mut delimiter: Vec<u8> = b";".to_vec();
    let mut seg_start = 0usize;
    let mut has_code = false;
    let mut i = 0usize;
    // Open BEGIN/CASE … END blocks in the current statement (EndWord dialects).
    let mut depth = 0i32;
    // A BEGIN opened in it: a statement led by DECLARE (Snowflake) keeps its
    // `;`s until its block has opened and closed.
    let mut opened = false;
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
            out.push(StatementSpan { start: from + lead, end: from + lead + trimmed.len(), sql: trimmed.to_string() });
        }
    };

    while i < n {
        // Line-level directives.
        if i == 0 || b[i - 1] == b'\n' {
            let line_end = text[i..].find('\n').map(|p| i + p).unwrap_or(n);
            let line = text[i..line_end].trim();
            let directive = match syntax.block_end {
                _ if syntax.delimiter_directive && line.len() > 10 && line.is_char_boundary(10) && line[..10].eq_ignore_ascii_case("delimiter ") => {
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
                opened = false;
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
                    Block::EndWord => depth > 0 || (!opened && syntax.block_starts.contains(&"DECLARE") && leading_keyword(seg) == "DECLARE"),
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
            opened = false;
            continue;
        }

        let c = b[i];
        if let Some((end, what)) = skip_token(text, i, syntax) {
            has_code |= what != Skipped::Comment;
            i = end;
            continue;
        }
        if !c.is_ascii_whitespace() {
            has_code = true;
        }

        match c {
            c if (c.is_ascii_alphabetic() || c == b'_')
                && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'$' || b[i - 1] == b'.')) =>
            {
                let mut j = i + 1;
                while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                let w = &text[i..j];
                if syntax.block_end == BlockEnd::EndWord {
                    if w.eq_ignore_ascii_case("BEGIN") {
                        let next = next_word(text, j);
                        let opens = !BEGIN_NOT_BLOCK.contains(&next.as_str()) && !text[j..].trim_start().starts_with(';');
                        depth += opens as i32;
                        opened |= opens;
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

pub(super) fn skip_line(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    i
}

/// Skip a quoted literal/identifier starting at `i`. Doubled quotes are
/// escapes; backslash escapes apply for MySQL string literals.
pub(super) fn skip_quoted(b: &[u8], i: usize, q: u8, backslash: bool) -> usize {
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
pub(super) fn dollar_tag(text: &str, i: usize) -> Option<&str> {
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
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty())
}

pub(crate) fn strip_leading_comments(mut s: &str) -> &str {
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
    words(strip_leading_comments(sql)).next().unwrap_or("").to_ascii_uppercase()
}

/// Pick the statement for "run statement at cursor". `cursor` is a byte
/// offset. If the cursor sits between statements, the preceding one wins.
pub fn statement_at(spans: &[StatementSpan], cursor: usize) -> Option<&StatementSpan> {
    spans.iter().find(|s| cursor >= s.start && cursor <= s.end).or_else(|| spans.iter().rev().find(|s| s.end < cursor)).or_else(|| spans.first())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectorKind::*;

    fn sqls(text: &str, k: ConnectorKind) -> Vec<String> {
        split_statements(text, k).into_iter().map(|s| s.sql).collect()
    }

    #[test]
    fn split_basic_and_quotes() {
        let t = "select 1; select ';' as x -- c;\n; select \"a;b\" from t;";
        assert_eq!(sqls(t, Postgres), vec!["select 1", "select ';' as x -- c;", "select \"a;b\" from t"]);
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
    fn dialect_strings_and_blocks_stay_whole() {
        let n = |sql: &str, k: ConnectorKind| split_statements(sql, k).into_iter().map(|s| s.sql).collect::<Vec<_>>();
        // Postgres and DuckDB E'…' strings: \' doesn't end them.
        for k in [ConnectorKind::Postgres, ConnectorKind::Duckdb] {
            assert_eq!(n("select E'it\\'s; x'; select 2", k), vec!["select E'it\\'s; x'", "select 2"], "{k:?}");
            assert_eq!(n("select e'a\\\\'; select 2", k).len(), 2, "{k:?}: an escaped backslash, then the end");
        }
        // Where E'…' isn't an escape string, \' ends the string: split as the server would.
        assert_eq!(n("select E'a\\'; drop table t; --'", ConnectorKind::Mssql).len(), 2);
        assert_eq!(n("select E'a\\'; drop table t; --'", ConnectorKind::Sqlite).len(), 2);
        // A column named e followed by a string is not affected elsewhere.
        assert_eq!(n("select e 'x'; select 2", ConnectorKind::Postgres).len(), 2);
        // Oracle q-quotes.
        assert_eq!(n("select q'[it's; x]' from dual;\nselect 2 from dual", ConnectorKind::Oracle), vec!["select q'[it's; x]' from dual", "select 2 from dual"]);
        assert_eq!(n("select Q'{a;'b}' from dual; select 1 from dual", ConnectorKind::Oracle).len(), 2);
        assert_eq!(n("select nq'!x;y!' from dual; select 1 from dual", ConnectorKind::Oracle).len(), 2);
        assert_eq!(n("select q'<unterminated; x", ConnectorKind::Oracle).len(), 1);
        // Snowflake Scripting blocks without $$.
        let sf = ConnectorKind::Snowflake;
        assert_eq!(n("BEGIN\n  LET x := 1;\n  RETURN x;\nEND;\nselect 2", sf), vec!["BEGIN\n  LET x := 1;\n  RETURN x;\nEND", "select 2"]);
        assert_eq!(
            n("DECLARE\n  c INT DEFAULT 0;\nBEGIN\n  c := 1;\n  RETURN c;\nEND;\nselect 3", sf),
            vec!["DECLARE\n  c INT DEFAULT 0;\nBEGIN\n  c := 1;\n  RETURN c;\nEND", "select 3"]
        );
        assert_eq!(n("begin;\ninsert into t values (1);\ncommit;", sf).len(), 3, "a transaction, not a block");
        assert_eq!(n("begin transaction;\ninsert into t values (1);\ncommit;", sf).len(), 3);
        assert_eq!(n("select 1; select case when a then 1 end from t; select 3", sf).len(), 3);
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
        assert_eq!(sqls("select `a;b` from t; select 'x\\';y'", Databricks).len(), 2);
        assert_eq!(sqls("select '''a;b''' # c;\n; select 2", Bigquery).len(), 2);
        let v = sqls("create procedure p() returns int language sql as $$ begin return 1; end $$; // c;\nselect 1", Snowflake);
        assert_eq!(v.len(), 2, "{v:?}");
    }

    #[test]
    fn split_procedure_bodies_and_nested_ends() {
        let v = |k: ConnectorKind, t: &str| sqls(t, k);
        // MySQL bodies without DELIMITER, DEFINER, CASE … END inside.
        assert_eq!(
            v(Mysql, "create procedure p() begin select 1; select 2; end;\nselect 3;"),
            vec!["create procedure p() begin select 1; select 2; end", "select 3"]
        );
        assert_eq!(v(Mysql, "create definer=`root`@`%` procedure p() begin select 1; end;\nselect 3;").len(), 2);
        assert_eq!(
            v(Mysql, "create procedure p() begin declare x int; set x = case when 1 then 2 else 3 end; if x > 1 then select 1; end if; end;\nselect 3;").len(),
            2
        );
        assert_eq!(v(Mysql, "create function f() returns int deterministic return 1;\nselect f();").len(), 2, "single-statement body");
        // Repeated DELIMITER switches.
        assert_eq!(
            v(
                Mysql,
                "delimiter //\ncreate procedure a() begin select 1; end //\ndelimiter ;\nselect 1;\ndelimiter $$\ncreate procedure b() begin select 2; end$$\ndelimiter ;\nselect 2;"
            ),
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
        assert_eq!(
            sqls("select 1\n\nselect 2\n\nupdate t set a = 1\n\nwhere b = 2", Postgres),
            vec!["select 1", "select 2", "update t set a = 1\n\nwhere b = 2"]
        );
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
}
