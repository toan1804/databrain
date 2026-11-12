use sqlparser::parser::Parser;

use super::classify::{Classification, StatementKind};
use super::{dialect_for, words};
use crate::types::ConnectorKind;

// ------------------------------------------------------------------ read-only

/// Session settings a read-only connection may still change: none of them
/// can make the session writable.
const READ_ONLY_SETTINGS: &[&str] = &[
    "search_path",
    "schema",
    "time",
    "timezone",
    "time_zone",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "datestyle",
    "intervalstyle",
    "application_name",
    "extra_float_digits",
    "names",
    "character_set_results",
    "max_execution_time",
    "net_read_timeout",
    "query_tag",
    "language",
    "dateformat",
    "datefirst",
    "nocount",
    "ansi_nulls",
    "quoted_identifier",
    "textsize",
    "arithabort",
    "lock_timeout_ms",
    "threads",
    "memory_limit",
];

/// Whether a read-only connection may run this statement. Only reads, plus
/// `USE …` and a few harmless `SET`s: transactions (`BEGIN READ WRITE`,
/// `COMMIT` ending Oracle's read-only transaction) and other session
/// statements (`SET default_transaction_read_only = off`, `ATTACH`,
/// `PRAGMA`) could make the session writable.
pub fn read_only_allows(sql: &str, c: &Classification) -> bool {
    match c.kind {
        StatementKind::Read => true,
        StatementKind::Session => {
            let mut w = words(sql).map(|w| w.to_ascii_lowercase());
            match w.next().as_deref() {
                // Not `USE ROLE`: another role may write. The word after
                // USE is read past comments (`use /*x*/ role …`).
                Some("use") => !matches!(word_after_use(sql).as_deref(), Some("role" | "secondary") | None),
                // A comment inside could hide a quote and with it a second
                // setting (`SET a='x' /* ' */, GLOBAL read_only = 0 /* ' */`):
                // not trusted.
                Some("set") if has_comment(super::splitter::strip_leading_comments(sql)) => false,
                Some("set") => set_names(sql).is_some_and(|names| !names.is_empty() && names.iter().all(|n| harmless_setting(n))),
                _ => false,
            }
        }
        _ => false,
    }
}

/// The word after the leading `USE`, lower case, past whitespace and any
/// comment (`--`, `/* */`, `//`, `#`, whichever the dialect has).
fn word_after_use(sql: &str) -> Option<String> {
    let body = super::splitter::strip_leading_comments(sql);
    let mut rest = body.get(body.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(body.len())..)?;
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix("--").or_else(|| rest.strip_prefix("//")).or_else(|| rest.strip_prefix('#')) {
            rest = r.split_once('\n').map_or("", |x| x.1);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |x| x.1);
        } else {
            break;
        }
    }
    words(rest).next().map(|w| w.to_ascii_lowercase())
}

/// Comment markers anywhere (strings included: being stricter only costs a
/// confirmation).
fn has_comment(sql: &str) -> bool {
    sql.contains("--") || sql.contains("/*") || sql.contains('#')
}

/// A setting `SET` may change on a read-only connection: `name` is as
/// written, lower-case, with its scope (`session time_zone`,
/// `@@global.read_only`). Any scope other than the session's is refused.
fn harmless_setting(name: &str) -> bool {
    let mut parts = name.split_whitespace();
    let mut first = parts.next().unwrap_or_default();
    if matches!(first, "session" | "local") {
        first = parts.next().unwrap_or_default();
    }
    if parts.next().is_some() {
        return false;
    }
    let n = first.strip_prefix("@@").map(|n| n.strip_prefix("session.").unwrap_or(n)).unwrap_or(first);
    let n = n.strip_prefix('@').unwrap_or(n);
    !n.contains('.') && READ_ONLY_SETTINGS.contains(&n)
}

/// The settings a `SET` changes, lower-case with their scope words: one for
/// `SET x = 1` / `SET x TO 1` / `SET nocount ON`, each of MySQL's
/// `SET a = 1, GLOBAL b = 0`, and each of T-SQL's `SET nocount, ansi_nulls ON`.
/// A comma inside a value (`SET search_path = a, b`) doesn't start one.
/// `None` when it can't be told (an unclosed quote or bracket).
fn set_names(sql: &str) -> Option<Vec<String>> {
    let body = super::splitter::strip_leading_comments(sql).trim();
    let body = body.get(3..)?.trim_end_matches(';');
    // Top-level pieces between commas, quote- and bracket-aware.
    let mut pieces = vec![String::new()];
    let (mut quote, mut depth) = (None::<char>, 0i32);
    for ch in body.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None => match ch {
                '\'' | '"' | '`' => quote = Some(ch),
                '[' => quote = Some(']'),
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    pieces.push(String::new());
                    continue;
                }
                _ => {}
            },
        }
        pieces.last_mut()?.push(ch);
    }
    if quote.is_some() || depth != 0 {
        return None;
    }
    // Where the name of a piece ends: `=`, `:=`, or ` TO ` (Postgres).
    let name_of = |p: &str| -> Option<String> {
        let l = p.to_ascii_lowercase();
        let end = l.find(":=").or_else(|| l.find('=')).or_else(|| l.find(" to "))?;
        Some(l[..end].trim().to_string())
    };
    let first = &pieces[0];
    Some(match name_of(first) {
        // `SET a = 1, b = 2`: a later piece is another setting when it has
        // its own `=`; otherwise it continues the value.
        Some(n) => std::iter::once(n).chain(pieces[1..].iter().filter_map(|p| name_of(p))).collect(),
        // T-SQL `SET nocount ON`, `SET nocount, xact_abort ON`, Postgres
        // `SET TIME ZONE 'UTC'`, MySQL `SET NAMES x COLLATE y`: every piece
        // starts with a name (after its scope word, if any).
        None => pieces
            .iter()
            .map(|p| {
                let l = p.to_ascii_lowercase();
                let mut w = l.split_whitespace();
                match w.next() {
                    Some(s @ ("session" | "local" | "global" | "persist" | "persist_only")) => format!("{s} {}", w.next().unwrap_or_default()),
                    Some(n) => n.to_string(),
                    None => String::new(),
                }
            })
            .collect(),
    })
}

/// Built-in functions that never write (common to the SQL dialects).
const PURE_FUNCTIONS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "coalesce",
    "nullif",
    "lower",
    "upper",
    "trim",
    "ltrim",
    "rtrim",
    "btrim",
    "length",
    "len",
    "char_length",
    "character_length",
    "octet_length",
    "substr",
    "substring",
    "concat",
    "concat_ws",
    "replace",
    "translate",
    "reverse",
    "round",
    "trunc",
    "truncate",
    "floor",
    "ceil",
    "ceiling",
    "abs",
    "sign",
    "mod",
    "sqrt",
    "power",
    "pow",
    "exp",
    "ln",
    "log",
    "log10",
    "log2",
    "pi",
    "random",
    "rand",
    "cast",
    "try_cast",
    "convert",
    "try_convert",
    "date_trunc",
    "date_part",
    "datepart",
    "datename",
    "extract",
    "now",
    "current_date",
    "current_timestamp",
    "current_time",
    "localtimestamp",
    "localtime",
    "getdate",
    "getutcdate",
    "sysdate",
    "systimestamp",
    "sysdatetime",
    "dateadd",
    "datediff",
    "date_add",
    "date_sub",
    "date_diff",
    "timestampdiff",
    "timestampadd",
    "to_char",
    "to_date",
    "to_timestamp",
    "to_number",
    "to_varchar",
    "to_decimal",
    "date",
    "time",
    "timestamp",
    "year",
    "month",
    "day",
    "hour",
    "minute",
    "second",
    "week",
    "quarter",
    "dayofweek",
    "dayofyear",
    "last_day",
    "add_months",
    "months_between",
    "date_format",
    "str_to_date",
    "strftime",
    "strptime",
    "age",
    "row_number",
    "rank",
    "dense_rank",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
    "ntile",
    "percent_rank",
    "cume_dist",
    "greatest",
    "least",
    "iff",
    "iif",
    "if",
    "ifnull",
    "isnull",
    "nvl",
    "nvl2",
    "zeroifnull",
    "decode",
    "listagg",
    "string_agg",
    "array_agg",
    "group_concat",
    "split_part",
    "split",
    "position",
    "strpos",
    "instr",
    "charindex",
    "locate",
    "left",
    "right",
    "lpad",
    "rpad",
    "format",
    "repeat",
    "regexp_replace",
    "regexp_substr",
    "regexp_like",
    "regexp_matches",
    "regexp_extract",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "variance",
    "var_pop",
    "var_samp",
    "median",
    "percentile_cont",
    "percentile_disc",
    "approx_count_distinct",
    "any_value",
    "bool_and",
    "bool_or",
    "every",
    "bit_and",
    "bit_or",
    "json_extract",
    "json_extract_path_text",
    "json_value",
    "json_query",
    "parse_json",
    "try_parse_json",
    "get_json_object",
    "to_json",
    "to_jsonb",
    "json_build_object",
    "jsonb_build_object",
    "json_agg",
    "jsonb_agg",
    "array_size",
    "array_length",
    "array_contains",
    "cardinality",
    "unnest",
    "flatten",
    "generate_series",
    "md5",
    "sha1",
    "sha2",
    "hash",
    "uuid",
    "gen_random_uuid",
    "newid",
    "current_user",
    "current_schema",
    "current_database",
    "user",
    "version",
    "typeof",
    "exists",
    "any",
    "all",
    "some",
    "grouping",
    "nvl",
    "chr",
    "char",
    "ascii",
    "initcap",
    "soundex",
    "difference",
    "space",
    "stuff",
    "quotename",
    "object_id",
    "db_name",
    "schema_name",
];

/// Functions `sql` calls that are not known built-ins (user functions,
/// procedures' wrappers, `pkg.fn`, `dblink`, …): on a database without a
/// read-only session mode, one of them could write.
pub fn unknown_functions(sql: &str, kind: ConnectorKind) -> Vec<String> {
    use sqlparser::ast::{Expr, ObjectName, TableFactor, Visit, Visitor};
    use std::ops::ControlFlow;
    struct V(Vec<String>);
    impl V {
        fn check(&mut self, name: &ObjectName) {
            let n = name.to_string();
            let plain = name.0.len() == 1 && PURE_FUNCTIONS.contains(&n.trim_matches(['"', '`', '[', ']']).to_ascii_lowercase().as_str());
            if !plain && !self.0.contains(&n) {
                self.0.push(n);
            }
        }
    }
    impl Visitor for V {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
            if let Expr::Function(f) = e {
                self.check(&f.name);
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_table_factor(&mut self, t: &TableFactor) -> ControlFlow<()> {
            match t {
                TableFactor::Table { name, args: Some(_), .. } | TableFactor::Function { name, .. } => self.check(name),
                _ => {}
            }
            ControlFlow::Continue(())
        }
    }
    if !kind.traits().language.is_sql() {
        return vec![];
    }
    match Parser::parse_sql(dialect_for(kind).as_ref(), sql) {
        Ok(stmts) => {
            let mut v = V(vec![]);
            let _ = stmts.visit(&mut v);
            v.0
        }
        // Can't tell: assume the worst.
        Err(_) => vec!["(SQL that could not be parsed)".into()],
    }
}

#[cfg(test)]
mod read_only_tests {
    use super::super::classify::classify;
    use super::*;
    use ConnectorKind::*;

    fn allows(sql: &str, k: ConnectorKind) -> bool {
        read_only_allows(sql, &classify(sql, k))
    }

    #[test]
    fn read_only_allows_reads_and_harmless_settings() {
        for sql in [
            "select 1",
            "with x as (select 1) select * from x",
            "explain select 1",
            "use sales",
            "set search_path = a, b",
            "SET SESSION time_zone = '+00:00'",
            "set @@session.max_execution_time = 1000",
        ] {
            assert!(allows(sql, Postgres) || allows(sql, Mysql), "{sql}");
        }
        assert!(allows("use warehouse wh", Snowflake));
        for (sql, k) in [
            ("insert into t values (1)", Postgres),
            ("with d as (delete from t returning *) select * from d", Postgres),
            ("select * into t2 from t", Postgres),
            ("explain analyze delete from t", Postgres),
            ("begin read write", Postgres),
            ("commit", Oracle),
            ("set default_transaction_read_only = off", Postgres),
            ("set session transaction read write", Mysql),
            ("use role sysadmin", Snowflake),
            ("use secondary roles all", Snowflake),
            ("attach 'x.db' as x", Duckdb),
            ("pragma query_only = 0", Sqlite),
            ("call p()", Postgres),
            ("exec dbo.p", Mssql),
        ] {
            assert!(!allows(sql, k), "{k:?}: {sql}");
        }
    }

    #[test]
    fn every_setting_of_a_set_is_checked() {
        for (k, sql) in [
            (Mysql, "SET time_zone = '+00:00', names = 'utf8mb4'"),
            (Mysql, "set @@session.time_zone = '+00:00', session max_execution_time = 1000"),
            (Mysql, "SET NAMES utf8mb4 COLLATE utf8mb4_0900_ai_ci"),
            (Postgres, "set search_path = a, b"),
            (Postgres, "set search_path to 'a,b', public"),
            (Postgres, "SET TIME ZONE 'UTC'"),
            (Postgres, "set application_name = 'x=1, y'"),
            (Mssql, "set nocount on"),
            (Mssql, "SET NOCOUNT, ANSI_NULLS ON"),
        ] {
            assert!(allows(sql, k), "{k:?}: {sql}");
        }
        for (k, sql) in [
            (Mysql, "SET time_zone = '+00:00', GLOBAL read_only = 0"),
            (Mysql, "set time_zone = '+00:00', @@global.read_only = 0"),
            (Mysql, "set names = 'utf8', sql_log_bin = 0"),
            (Mysql, "SET PERSIST max_connections = 10"),
            (Mysql, "set global time_zone = '+00:00'"),
            (Mysql, "set time_zone = '+00:00', session transaction_read_only = 0"),
            (Postgres, "set search_path = a, b; set default_transaction_read_only = off"),
            (Mssql, "SET NOCOUNT, IMPLICIT_TRANSACTIONS ON"),
            (Mssql, "set nocount on, xact_abort on"),
            (Postgres, "set search_path = 'a"),
            // A comment hiding a quote, and with it a second setting.
            (Mysql, "SET time_zone='+00:00' /* ' */, GLOBAL read_only = 0 /* ' */"),
            (Mysql, "SET time_zone='+00:00' # '\n, transaction_read_only=0 # '\n"),
            (Mysql, "set time_zone = '+00:00' -- ', global general_log = 1"),
        ] {
            assert!(!allows(sql, k), "{k:?}: {sql}");
        }
    }

    #[test]
    fn finds_functions_that_are_not_built_ins() {
        assert!(unknown_functions("select count(*), lower(name), coalesce(a, 0) from t", Mssql).is_empty());
        assert_eq!(unknown_functions("select dbo.f(1), g(x) from t", Mssql), vec!["dbo.f", "g"]);
        assert_eq!(unknown_functions("select * from dbo.tvf(1)", Mssql), vec!["dbo.tvf"]);
        // A built-in's name in a schema is not the built-in.
        assert_eq!(unknown_functions("select evil.lower(x) from t", Postgres), vec!["evil.lower"]);
        assert_eq!(unknown_functions("select * from t where id in (select pkg.f() from dual)", Oracle), vec!["pkg.f"]);
        assert_eq!(unknown_functions("selec f(", Mssql), vec!["(SQL that could not be parsed)"]);
        assert!(unknown_functions("db.c.find({})", Mongodb).is_empty(), "not SQL");
    }
}
