use serde::Serialize;
use sqlparser::ast::{Query, SetExpr, Statement};
use sqlparser::parser::Parser;

use super::{dialect_for, leading_keyword, words};
use crate::types::ConnectorKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, ts_rs::TS)]
#[ts(export)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
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
    let c = classify_sql(sql, kind);
    // MySQL and MariaDB run what is inside `/*! … */` and `/*M! … */`
    // (MariaDB only), which the parser takes for a comment or reads only in
    // part: `select * from t /*M! into outfile '/tmp/x' */` writes a file.
    // What runs there can't be told, so it is asked about.
    if kind == ConnectorKind::Mysql && (sql.contains("/*!") || sql.contains("/*M!") || sql.contains("/*m!")) {
        return Classification { kind: c.kind.max(StatementKind::Unknown), ..c };
    }
    // BigQuery and ClickHouse read ``\` `` inside a quoted name as an
    // escaped backtick; the parser ends the name there. The text as the
    // server splits it decides: ``select 1 `a\`, `; drop table t`` is two.
    if kind.traits().syntax.backslash_in_backticks && sql.contains("\\`") {
        let pieces = super::split_statements(sql, kind);
        if pieces.len() > 1 {
            let worst = pieces.iter().map(|p| classify_sql(&p.sql, kind).kind).max().unwrap_or(StatementKind::Read);
            return Classification { kind: c.kind.max(worst).max(StatementKind::Unknown), ..c };
        }
    }
    // MySQL reads `\'` inside a string as an escaped quote, unless the server
    // runs with `NO_BACKSLASH_ESCAPES` (a `sql_mode` DataBrain doesn't
    // know): then `select 'a\' into outfile ', 1 -- '` ends the string at
    // `\'` and writes a file, and `select 'a\'; drop table t; -- '` runs a
    // DROP. Such a server reads `'a\'` as the parser reads `'a\\'`, so the
    // text is also classified with every backslash doubled, and the
    // stricter reading wins.
    if kind == ConnectorKind::Mysql && sql.contains('\\') {
        // Each statement as DataBrain splits the text, read the other way.
        let other = super::split_statements(sql, kind).iter().map(|p| no_backslash_kind(&p.sql, kind)).max().unwrap_or(StatementKind::Read);
        return Classification { kind: c.kind.max(other), ..c };
    }
    // Several statements as DataBrain splits the text (it reads strings as
    // the server does): the worst of them counts, whatever the parser saw.
    // The parser may read a string differently and see one statement:
    // DuckDB's `e'\''` is one quote to the server, the end of `e'\'` and a
    // new string to the parser, so `select e'\''; drop table t` was a read.
    if sql.contains(';') {
        let pieces = super::split_statements(sql, kind);
        if pieces.len() > 1 {
            let worst = pieces.iter().map(|p| classify_sql(&p.sql, kind).kind).max().unwrap_or(StatementKind::Read);
            return Classification { kind: c.kind.max(worst), ..c };
        }
    }
    c
}

/// How a MySQL server with `NO_BACKSLASH_ESCAPES` reads one statement
/// (`Read` when it would reject it and run nothing).
fn no_backslash_kind(sql: &str, kind: ConnectorKind) -> StatementKind {
    if !sql.contains('\\') {
        return StatementKind::Read;
    }
    let doubled = sql.replace('\\', "\\\\");
    // More than one statement that way: a server with multi-statements on
    // runs them one after the other.
    if super::split_statements(&doubled, kind).len() > 1 {
        return StatementKind::Unknown;
    }
    // One statement whose string never ends that way (`'O\'Brien'`): the
    // server rejects it and runs nothing.
    if sqlparser::tokenizer::Tokenizer::new(dialect_for(kind).as_ref(), &doubled).tokenize().is_err() {
        return StatementKind::Read;
    }
    classify_sql(&doubled, kind).kind
}

fn classify_sql(sql: &str, kind: ConnectorKind) -> Classification {
    let keyword = leading_keyword(sql);
    let dialect = dialect_for(kind);
    match Parser::parse_sql(dialect.as_ref(), sql) {
        Ok(stmts) if !stmts.is_empty() => {
            let mut out = Classification { kind: StatementKind::Read, missing_where: false, keyword: keyword.clone() };
            for s in &stmts {
                let (k, mw) = classify_ast(s);
                out.kind = out.kind.max(k);
                out.missing_where |= mw;
            }
            out.kind = check_control(out.kind, &keyword, sql);
            // `select pg_terminate_backend(…)` acts like `KILL`.
            if out.kind < StatementKind::Dml && (stmts.iter().any(calls_acting_function) || next_value_for(sql, dialect.as_ref())) {
                out.kind = StatementKind::Other;
            }
            // What the parser accepted may not be what the server runs:
            // a T-SQL statement word read as an alias (`select 1 shutdown`:
            // T-SQL needs no separator), Snowflake's `->>` chaining a second
            // statement, or a function name hidden by Unicode escapes.
            // A Snowflake procedure called as a table function: whatever
            // else the text holds, it may write.
            if snowflake_table_call(sql, kind) {
                out.kind = out.kind.max(StatementKind::Unknown);
            }
            if out.kind.is_read() && (hides_statement(&stmts, kind) || chains_statements(sql, kind) || escaped_identifier(sql)) {
                out.kind = StatementKind::Unknown;
            }
            out
        }
        _ => {
            let mut k = keyword_kind(&keyword);
            if k == StatementKind::Read && keyword == "WITH" && contains_dml_word(sql) {
                k = StatementKind::Dml;
            }
            // Unparseable but looks like a read: the parser couldn't say what
            // it does, so it isn't trusted as one (T-SQL runs `SELECT 1 DENY
            // …` as two statements without a separator, and a list of words
            // that act always misses some).
            if k == StatementKind::Read {
                k = StatementKind::Unknown;
            }
            // Unparseable session or transaction control: what follows the
            // first word can't be told (a T-SQL batch without `;` runs
            // `USE sales` then `DISABLE TRIGGER …`, `DENY …`,
            // `RECONFIGURE`), and a list of words that act always misses
            // some, so it isn't trusted as control.
            if k == StatementKind::Session || (k == StatementKind::Transaction && !transaction_grammar(sql)) {
                k = StatementKind::Unknown;
            }
            Classification { kind: k, missing_where: false, keyword }
        }
    }
}

/// T-SQL statements that a batch may start without `;` and that sqlparser
/// accepts as an alias after a select item or table (`select 1 shutdown`,
/// `select * from t revert`).
const TSQL_STATEMENTS: &[&str] = &[
    "SHUTDOWN",
    "RECONFIGURE",
    "REVERT",
    "CHECKPOINT",
    "KILL",
    "DENY",
    "GRANT",
    "REVOKE",
    "EXEC",
    "EXECUTE",
    "DBCC",
    "BACKUP",
    "RESTORE",
    "WAITFOR",
    "USE",
    "SET",
    "DECLARE",
    "PRINT",
    "RAISERROR",
    "THROW",
    "RETURN",
    "BREAK",
    "CONTINUE",
    "GOTO",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SAVE",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "DROP",
    "ALTER",
    "CREATE",
    "TRUNCATE",
    "DISABLE",
    "ENABLE",
    "SETUSER",
    "WRITETEXT",
    "UPDATETEXT",
    "READTEXT",
    "SEND",
    "RECEIVE",
    "MOVE",
    "GET",
    "BULK",
    "OPEN",
    "CLOSE",
    "FETCH",
    "DEALLOCATE",
    "IF",
    "WHILE",
    "ELSE",
    "END",
    "CALL",
    "LOAD",
    "GO",
    "ADD",
    "RENAME",
    "COPY",
];

/// SQL Server: an unquoted alias that is a statement word. sqlparser reads
/// `select 1 shutdown` as one SELECT; the server runs a SELECT, then
/// SHUTDOWN.
fn hides_statement(stmts: &[Statement], kind: ConnectorKind) -> bool {
    use sqlparser::ast::{Ident, Query, SelectItem, TableFactor, Visit, Visitor};
    use std::ops::ControlFlow;
    if kind != ConnectorKind::Mssql {
        return false;
    }
    fn word(i: &Ident) -> ControlFlow<()> {
        if i.quote_style.is_none() && TSQL_STATEMENTS.iter().any(|w| i.value.eq_ignore_ascii_case(w)) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
    fn items(e: &SetExpr) -> ControlFlow<()> {
        match e {
            SetExpr::Select(s) => {
                for it in &s.projection {
                    match it {
                        SelectItem::ExprWithAlias { alias, .. } => word(alias)?,
                        SelectItem::ExprWithAliases { aliases, .. } => aliases.iter().try_for_each(word)?,
                        _ => {}
                    }
                }
                ControlFlow::Continue(())
            }
            SetExpr::SetOperation { left, right, .. } => {
                items(left)?;
                items(right)
            }
            _ => ControlFlow::Continue(()),
        }
    }
    struct V;
    impl Visitor for V {
        type Break = ();
        fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
            items(&q.body)
        }
        fn pre_visit_table_factor(&mut self, t: &TableFactor) -> ControlFlow<()> {
            use TableFactor as T;
            let alias = match t {
                T::Table { alias, .. }
                | T::Derived { alias, .. }
                | T::TableFunction { alias, .. }
                | T::Function { alias, .. }
                | T::UNNEST { alias, .. }
                | T::JsonTable { alias, .. }
                | T::OpenJsonTable { alias, .. }
                | T::NestedJoin { alias, .. }
                | T::Pivot { alias, .. }
                | T::Unpivot { alias, .. }
                | T::MatchRecognize { alias, .. }
                | T::XmlTable { alias, .. } => alias.as_ref(),
                _ => None,
            };
            match alias {
                Some(a) => word(&a.name),
                None => ControlFlow::Continue(()),
            }
        }
    }
    stmts.iter().any(|s| s.visit(&mut V).is_break())
}

/// Snowflake's flow operator `->>` runs the statement after it on the
/// result of the one before (`select 1 ->> delete from t`); sqlparser reads
/// it as the JSON operator. Any `->>` token there means more than one
/// statement.
fn chains_statements(sql: &str, kind: ConnectorKind) -> bool {
    use sqlparser::tokenizer::{Token, Tokenizer};
    if kind != ConnectorKind::Snowflake {
        return false;
    }
    match Tokenizer::new(dialect_for(kind).as_ref(), sql).tokenize() {
        Ok(tokens) => tokens.iter().any(|t| matches!(t, Token::LongArrow)),
        Err(_) => sql.contains("->>"),
    }
}

/// Snowflake table functions DataBrain knows only read; any other name in
/// `TABLE(name(…))` may be a stored procedure, which a SELECT can call that
/// way (and which may write).
const SNOWFLAKE_TABLE_FUNCTIONS: &[&str] = &[
    "FLATTEN",
    "RESULT_SCAN",
    "GENERATOR",
    "SPLIT_TO_TABLE",
    "STRTOK_SPLIT_TO_TABLE",
    "INFER_SCHEMA",
    "VALIDATE",
    "EXTERNAL_TABLE_FILES",
    "EXTERNAL_TABLE_FILE_REGISTRATION_HISTORY",
    "QUERY_HISTORY",
    "QUERY_HISTORY_BY_SESSION",
    "QUERY_HISTORY_BY_USER",
    "QUERY_HISTORY_BY_WAREHOUSE",
    "LOGIN_HISTORY",
    "LOGIN_HISTORY_BY_USER",
    "COPY_HISTORY",
    "TASK_HISTORY",
    "TASK_DEPENDENTS",
    "WAREHOUSE_LOAD_HISTORY",
    "WAREHOUSE_METERING_HISTORY",
    "AUTOMATIC_CLUSTERING_HISTORY",
    "DATABASE_STORAGE_USAGE_HISTORY",
    "STAGE_STORAGE_USAGE_HISTORY",
    "PIPE_USAGE_HISTORY",
    "POLICY_REFERENCES",
    "TAG_REFERENCES",
    "TAG_REFERENCES_ALL_COLUMNS",
    "REST_EVENT_HISTORY",
    "STAGE_DIRECTORY_FILE_REGISTRATION_HISTORY",
    "DATA_TRANSFER_HISTORY",
    "SERVERLESS_TASK_HISTORY",
    "MATERIALIZED_VIEW_REFRESH_HISTORY",
    "SEARCH_OPTIMIZATION_HISTORY",
    "REPLICATION_USAGE_HISTORY",
    "DYNAMIC_TABLE_REFRESH_HISTORY",
    "DYNAMIC_TABLE_GRAPH_HISTORY",
    "DYNAMIC_TABLES",
    "ALERT_HISTORY",
    "GET_OBJECT_REFERENCES",
];

/// Snowflake: `TABLE(name(…))` with a name that isn't a known read-only
/// table function or one from `INFORMATION_SCHEMA` / `SNOWFLAKE.`.
fn snowflake_table_call(sql: &str, kind: ConnectorKind) -> bool {
    if kind != ConnectorKind::Snowflake {
        return false;
    }
    let code = super::code_text(sql, kind).to_ascii_uppercase();
    let b = code.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut from = 0;
    while let Some(p) = code[from..].find("TABLE") {
        let at = from + p;
        from = at + 5;
        if at > 0 && (b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_' || b[at - 1] == b'.') {
            continue;
        }
        let mut i = skip_ws(at + 5);
        if b.get(i) != Some(&b'(') {
            continue;
        }
        i = skip_ws(i + 1);
        // The name's parts, with whitespace (and comments, blanked by
        // code_text) allowed around the dots: `myschema . evil_proc()`.
        let mut parts: Vec<&str> = Vec::new();
        loop {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'"' | b'$')) {
                i += 1;
            }
            parts.push(&code[start..i]);
            let j = skip_ws(i);
            if b.get(j) != Some(&b'.') {
                break;
            }
            i = skip_ws(j + 1);
        }
        if parts.iter().all(|p| p.is_empty()) || b.get(skip_ws(i)) != Some(&b'(') {
            continue;
        }
        // Built-in table functions aren't qualified, except under
        // INFORMATION_SCHEMA or SNOWFLAKE: `myschema.flatten` is the user's.
        // A quoted name is never trusted.
        let quoted = parts.iter().any(|p| p.contains(super::splitter::QUOTED_WORD));
        let trusted = match parts.as_slice() {
            [f] => SNOWFLAKE_TABLE_FUNCTIONS.contains(f),
            [first, ..] if *first == "SNOWFLAKE" => true,
            [.., schema, _] => *schema == "INFORMATION_SCHEMA",
            _ => false,
        };
        if quoted || !trusted {
            return true;
        }
    }
    false
}

/// `U&"…"` identifiers spell a name with escapes
/// (`U&"pg\005Fterminate\005Fbackend"(1)`), so the name lists above can't
/// see it. Matched anywhere, strings included: being stricter costs a
/// confirmation.
fn escaped_identifier(sql: &str) -> bool {
    sql.as_bytes().windows(3).any(|w| w[0].eq_ignore_ascii_case(&b'u') && w[1] == b'&' && w[2] == b'"')
}

fn classify_ast(s: &Statement) -> (StatementKind, bool) {
    use StatementKind as K;
    match s {
        Statement::Query(q) => (query_kind(q), false),
        Statement::Insert(_) | Statement::Merge(_) => (K::Dml, false),
        Statement::Update(u) => (K::Dml, u.selection.is_none()),
        Statement::Delete(d) => (K::Dml, d.selection.is_none()),
        // The inner statement decides, ANALYZE or not: `EXPLAIN (ANALYZE) …`,
        // `EXPLAIN (ANALYZE true, BUFFERS) …` and other option lists leave
        // `analyze` false and put it in `options`, and engines differ on
        // which EXPLAIN forms run the statement. A plain EXPLAIN of a write
        // counts as that write too: being stricter costs a confirmation.
        Statement::Explain { statement, .. } => {
            let (inner, missing_where) = classify_ast(statement);
            (K::Read.max(inner), missing_where)
        }
        Statement::ExplainTable { .. } => (K::Read, false),
        // The statement's own keyword: in `select 1; exec p`, EXEC's is EXEC.
        _ => (keyword_kind(&leading_keyword(&s.to_string())), false),
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
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_) => StatementKind::Dml,
        #[allow(unreachable_patterns)]
        _ => StatementKind::Unknown,
    }
}

pub(crate) fn keyword_kind(kw: &str) -> StatementKind {
    use StatementKind as K;
    match kw {
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "VALUES" | "TABLE" => K::Read,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" | "UPSERT" | "COPY" | "LOAD" | "CALL" | "EXEC" | "EXECUTE" | "DO" | "HANDLER" => K::Dml,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "COMMENT" | "GRANT" | "REVOKE" => K::Ddl,
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "END" | "ABORT" | "XA" => K::Transaction,
        "SET" | "USE" | "RESET" | "ATTACH" | "DETACH" | "PRAGMA" | "DISCARD" | "LISTEN" | "UNLISTEN" => K::Session,
        "" => K::Unknown,
        _ => K::Other,
    }
}

/// Session and transaction control is trusted (no production prompt, and some
/// `USE`/`SET` pass a read-only connection), so it must be only that:
/// - A T-SQL batch without `;` (`USE master\nDROP DATABASE prod`,
///   `set nocount on\nkill 52`) takes its first word's kind, so any later
///   word that acts makes it `Unknown`. Other control words (`SET`, `USE`,
///   `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `RESET`) may follow.
/// - `BEGIN`, `START` and `END` also open and close blocks or start
///   replication (`BEGIN DELETE FROM t END`, Oracle `BEGIN pkg.proc; END;`,
///   MySQL `START REPLICA`): they are transaction control only in their
///   transaction forms.
fn check_control(kind: StatementKind, keyword: &str, sql: &str) -> StatementKind {
    if !matches!(kind, StatementKind::Session | StatementKind::Transaction) {
        return kind;
    }
    let rest: Vec<&str> = words(super::splitter::strip_leading_comments(sql)).skip(1).collect();
    let second = rest.first().map(|w| w.to_ascii_uppercase());
    let second = second.as_deref();
    let transaction_form = match keyword {
        "BEGIN" => matches!(
            second,
            None | Some("TRAN" | "TRANSACTION" | "WORK" | "ISOLATION" | "READ" | "NOT" | "DEFERRABLE" | "DEFERRED" | "IMMEDIATE" | "EXCLUSIVE" | "DISTRIBUTED")
        ),
        "START" => second == Some("TRANSACTION"),
        "END" => matches!(second, None | Some("TRANSACTION" | "WORK")),
        _ => true,
    };
    const CONTROL: &[&str] = &["SET", "USE", "RESET", "COMMIT", "ROLLBACK", "SAVEPOINT"];
    let acts = rest
        .iter()
        .any(|w| !CONTROL.iter().any(|c| w.eq_ignore_ascii_case(c)) && (contains_dml_word(w) || contains_action_word(w) || w.eq_ignore_ascii_case("END")));
    if !transaction_form || acts { StatementKind::Unknown } else { kind }
}

/// Unparsed transaction control that is only that: every word after the
/// first is transaction grammar (`START TRANSACTION WITH CONSISTENT
/// SNAPSHOT`, `BEGIN DISTRIBUTED TRANSACTION`), except a transaction name as
/// the last word after `TRAN`/`TRANSACTION` (`begin tran t1`) that isn't a
/// statement of its own (`begin tran` then `shutdown`).
fn transaction_grammar(sql: &str) -> bool {
    const GRAMMAR: &[&str] = &[
        "TRAN",
        "TRANSACTION",
        "WORK",
        "ISOLATION",
        "LEVEL",
        "READ",
        "WRITE",
        "ONLY",
        "COMMITTED",
        "UNCOMMITTED",
        "REPEATABLE",
        "SERIALIZABLE",
        "SNAPSHOT",
        "WITH",
        "CONSISTENT",
        "DEFERRABLE",
        "NOT",
        "DEFERRED",
        "IMMEDIATE",
        "EXCLUSIVE",
        "DISTRIBUTED",
        "AND",
        "NO",
        "CHAIN",
        "RELEASE",
        "TO",
        "SAVEPOINT",
        "MARK",
        "DELAYED_DURABILITY",
        "ON",
        "OFF",
    ];
    const STATEMENTS: &[&str] = &["SHUTDOWN", "RECONFIGURE", "CHECKPOINT", "BEGIN", "COMMIT", "ROLLBACK", "END", "RETURN", "BREAK", "CONTINUE", "GO"];
    let w: Vec<String> = words(super::splitter::strip_leading_comments(sql)).skip(1).map(|w| w.to_ascii_uppercase()).collect();
    let is = |w: &str, list: &[&str]| list.contains(&w);
    w.iter().enumerate().all(|(i, x)| {
        is(x, GRAMMAR)
            || (i + 1 == w.len()
                && i > 0
                && matches!(w[i - 1].as_str(), "TRAN" | "TRANSACTION")
                && !is(x, STATEMENTS)
                && !contains_action_word(x)
                && !contains_dml_word(x))
    })
}

/// Words that act (run code, change state or permissions, reach files)
/// besides INSERT/UPDATE/DELETE…: unparsed SQL with one isn't a read.
fn contains_action_word(sql: &str) -> bool {
    const ACTION: &[&str] = &[
        "EXEC",
        "EXECUTE",
        "CALL",
        "DO",
        "GRANT",
        "REVOKE",
        "DECLARE",
        "BEGIN",
        "WAITFOR",
        "KILL",
        "SHUTDOWN",
        "BACKUP",
        "RESTORE",
        "DBCC",
        "BULK",
        "OPENROWSET",
        "OPENQUERY",
        "OPENDATASOURCE",
        "SET",
        "USE",
        "COPY",
        "LOAD",
        "ATTACH",
        "DETACH",
        "PRAGMA",
        "VACUUM",
        "REPLACE",
        "UPSERT",
        "RENAME",
        "COMMENT",
        "HANDLER",
        "LOCK",
        "UNLOCK",
        "COMMIT",
        "ROLLBACK",
        "SAVEPOINT",
        "RESET",
        "DISCARD",
        "REFRESH",
        "IMPORT",
        "EXPORT",
        "UNLOAD",
        "PUT",
        "REMOVE",
        "CHECKPOINT",
        "CLUSTER",
        "REINDEX",
        "SYSTEM",
    ];
    words(sql).any(|w| ACTION.iter().any(|k| w.eq_ignore_ascii_case(k)))
}

/// Built-in functions that change server state, end sessions, change data
/// or write files, callable from a `SELECT` (`select pg_terminate_backend(42)`,
/// `select setval('s', 1)`, Snowflake `select system$cancel_query('…')`).
/// Such a statement isn't a read: it gets a production prompt and is refused
/// on a read-only connection.
const ACTING_FUNCTIONS: &[&str] = &[
    // Postgres: sessions, server and replication.
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_promote",
    "pg_switch_wal",
    "pg_switch_xlog",
    "pg_create_restore_point",
    "pg_start_backup",
    "pg_stop_backup",
    "pg_backup_start",
    "pg_backup_stop",
    "pg_wal_replay_pause",
    "pg_wal_replay_resume",
    "pg_xlog_replay_pause",
    "pg_xlog_replay_resume",
    "pg_create_physical_replication_slot",
    "pg_create_logical_replication_slot",
    "pg_drop_replication_slot",
    "pg_copy_physical_replication_slot",
    "pg_copy_logical_replication_slot",
    "pg_replication_slot_advance",
    "pg_logical_slot_get_changes",
    "pg_logical_slot_get_binary_changes",
    "pg_logical_emit_message",
    "pg_replication_origin_create",
    "pg_replication_origin_drop",
    "pg_replication_origin_advance",
    "pg_replication_origin_session_setup",
    "pg_log_backend_memory_contexts",
    "pg_stat_reset",
    "pg_stat_reset_shared",
    "pg_stat_reset_single_table_counters",
    "pg_stat_reset_single_function_counters",
    "pg_stat_reset_slru",
    "pg_stat_reset_replication_slot",
    "pg_stat_statements_reset",
    "pg_import_system_collations",
    "pg_notify",
    "set_config",
    // SQL in another database or a linked server, which may write: SQL
    // Server `openquery`/`openrowset`/`opendatasource`, DuckDB's attached
    // Postgres and MySQL, BigQuery federated queries.
    "openquery",
    "openrowset",
    "opendatasource",
    "postgres_query",
    "postgres_execute",
    "mysql_query",
    "mysql_execute",
    "external_query",
    // Postgres: data, sequences, files and other databases.
    "nextval",
    "setval",
    "lo_import",
    "lo_export",
    "lo_unlink",
    "lo_create",
    "lo_creat",
    "lo_put",
    "lo_from_bytea",
    "lo_truncate",
    "pg_file_write",
    "pg_file_rename",
    "pg_file_unlink",
    "pg_file_sync",
    "dblink",
    "dblink_exec",
    "dblink_send_query",
    "lowrite",
];

/// Snowflake `system$` functions that only read: every other `system$`
/// call counts as acting (`system$cancel_query`, `system$send_email`,
/// `system$disable_database_replication`, …). Snowflake has no read-only
/// session mode, so this is the only guard there; a new or unknown one
/// gets the production prompt and is refused on a read-only connection.
const SNOWFLAKE_READS: &[&str] = &[
    "system$clustering_information",
    "system$clustering_depth",
    "system$clustering_ratio",
    "system$typeof",
    "system$explain_plan_json",
    "system$explain_json_to_text",
    "system$get_tag",
    "system$get_tag_allowed_values",
    "system$get_tag_on_current_column",
    "system$get_tag_on_current_table",
    "system$pipe_status",
    "system$stream_has_data",
    "system$stream_get_table_timestamp",
    "system$last_change_commit_time",
    "system$current_user_task_name",
    "system$get_predecessor_return_value",
    "system$task_dependents",
    "system$database_refresh_progress",
    "system$database_refresh_progress_by_job",
    "system$estimate_search_optimization_costs",
    "system$whitelist",
    "system$allowlist",
    "system$whitelist_privatelink",
    "system$allowlist_privatelink",
    "system$get_privatelink_config",
    "system$is_listing_triggered",
    "system$get_snowflake_platform_info",
    "system$estimate_query_acceleration",
    "system$behavior_change_bundle_status",
    "system$show_active_behavior_change_bundles",
    "system$get_task_graph_config",
    "system$task_runtime_info",
    "system$get_service_status",
    "system$get_service_logs",
    "system$external_table_pipe_status",
    "system$get_directory_table_status",
    "system$client_version_info",
    "system$wait",
    "system$get_classification_result",
    "system$get_login_failure_details",
    "system$get_privatelink_authorized_endpoints",
];

fn is_acting_function(name: &str) -> bool {
    let name = name.trim_matches(['"', '`', '[', ']']).to_ascii_lowercase();
    if name.starts_with("system$") {
        return !SNOWFLAKE_READS.contains(&name.as_str());
    }
    ACTING_FUNCTIONS.contains(&name.as_str())
}

fn calls_acting_function(s: &Statement) -> bool {
    use sqlparser::ast::{Expr, ObjectName, TableFactor, Visit, Visitor};
    use std::ops::ControlFlow;
    struct V;
    fn acts(name: &ObjectName) -> ControlFlow<()> {
        // The function's own name: `pg_catalog.pg_terminate_backend` too.
        match name.0.last() {
            Some(part) if is_acting_function(&part.to_string()) => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    }
    impl Visitor for V {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
            match e {
                Expr::Function(f) => acts(&f.name),
                // Oracle `orders_seq.nextval` advances the sequence.
                Expr::CompoundIdentifier(parts) if parts.last().is_some_and(|p| p.value.eq_ignore_ascii_case("nextval")) => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            }
        }
        fn pre_visit_table_factor(&mut self, t: &TableFactor) -> ControlFlow<()> {
            match t {
                TableFactor::Table { name, args: Some(_), .. } | TableFactor::Function { name, .. } => acts(name),
                _ => ControlFlow::Continue(()),
            }
        }
    }
    s.visit(&mut V).is_break()
}

/// SQL Server / standard `NEXT VALUE FOR seq`, which advances the sequence
/// (sqlparser reads it as columns and aliases). Also inside a string: a
/// read is never claimed when unsure.
fn next_value_for(sql: &str, dialect: &dyn sqlparser::dialect::Dialect) -> bool {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let is = |w: &&str, k: &str| w.eq_ignore_ascii_case(k);
    let three = |w: &[&str]| w.windows(3).any(|w| is(&w[0], "next") && is(&w[1], "value") && is(&w[2], "for"));
    // The words as written, comments and strings included: never a miss
    // where the words stand side by side.
    if three(&words(sql).collect::<Vec<_>>()) {
        return true;
    }
    // The dialect's tokens without whitespace and comments
    // (`next /* c */ value for s`); strings and quoted names stay whole, and
    // block comments nest only where the dialect's do.
    match Tokenizer::new(dialect, sql).tokenize() {
        Ok(tokens) => {
            let w: Vec<&str> = tokens
                .iter()
                .filter(|t| !matches!(t, Token::Whitespace(_)))
                .map(|t| match t {
                    Token::Word(w) if w.quote_style.is_none() => w.value.as_str(),
                    _ => "",
                })
                .collect();
            three(&w)
        }
        // Not tokenizable: any comment could hide the gap, so the three words
        // anywhere in order are enough.
        Err(_) => {
            let w: Vec<&str> = words(sql).collect();
            let at = |k: &str, from: usize| w.iter().skip(from).position(|x| is(x, k)).map(|i| i + from + 1);
            at("next", 0).and_then(|i| at("value", i)).and_then(|i| at("for", i)).is_some()
        }
    }
}

fn contains_dml_word(sql: &str) -> bool {
    words(sql).any(|w| ["INSERT", "UPDATE", "DELETE", "MERGE", "INTO", "DROP", "ALTER", "CREATE", "TRUNCATE"].iter().any(|k| w.eq_ignore_ascii_case(k)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectorKind::*;

    #[test]
    fn classify_statements() {
        let c = |s: &str| classify(s, Postgres).kind;
        assert_eq!(c("select * from t"), StatementKind::Read);
        assert_eq!(c("with x as (select 1) select * from x"), StatementKind::Read);
        assert_eq!(c("with d as (delete from t returning *) select * from d"), StatementKind::Dml);
        assert_eq!(c("insert into t values (1)"), StatementKind::Dml);
        assert_eq!(c("drop table t"), StatementKind::Ddl);
        assert_eq!(c("select * into t2 from t"), StatementKind::Ddl);
        assert_eq!(c("begin"), StatementKind::Transaction);
        assert_eq!(c("explain select 1"), StatementKind::Read);
        assert_eq!(c("explain analyze delete from t"), StatementKind::Dml);
        // Postgres option lists: sqlparser leaves `analyze` false and puts it in `options`.
        assert_eq!(c("EXPLAIN (ANALYZE) DELETE FROM t"), StatementKind::Dml);
        assert_eq!(c("explain (analyze true, buffers) update t set a = 1 where id = 2"), StatementKind::Dml);
        assert_eq!(c("explain (analyze, format json) insert into t select * from s"), StatementKind::Dml);
        assert_eq!(c("explain (analyze) create table t2 as select 1"), StatementKind::Ddl);
        assert!(classify("EXPLAIN (ANALYZE) DELETE FROM t", Postgres).missing_where);
        // Without ANALYZE too: the statement explained decides.
        assert_eq!(c("explain delete from t"), StatementKind::Dml);
        assert_eq!(c("explain (format json) select 1"), StatementKind::Read);
        assert_eq!(c("explain (analyze) select * from t"), StatementKind::Read);
        assert_eq!(c("explain (analyze) with d as (delete from t returning *) select * from d"), StatementKind::Dml);
        for (k, sql) in [(Mysql, "EXPLAIN ANALYZE DELETE FROM t"), (Duckdb, "EXPLAIN ANALYZE DELETE FROM t"), (Snowflake, "EXPLAIN DELETE FROM t")] {
            assert_eq!(classify(sql, k).kind, StatementKind::Dml, "{k:?}: {sql}");
        }
        assert!(classify("delete from t", Postgres).missing_where);
        assert!(!classify("update t set a=1 where id=2", Postgres).missing_where);
        assert_eq!(classify("show tables", Mysql).kind, StatementKind::Read);
        assert_eq!(c("totally not sql ;;"), StatementKind::Other);
    }

    #[test]
    fn control_statements_that_act_are_unknown() {
        // T-SQL batches without `;`, blocks, and BEGIN/START/END that aren't transactions.
        for (k, sql) in [
            (Mssql, "USE master\nDROP DATABASE prod"),
            (Mssql, "use master\nshutdown"),
            (Mssql, "set nocount on\ndelete from t"),
            (Mssql, "set nocount on\nkill 52"),
            (Mssql, "USE sales\nDISABLE TRIGGER audit_orders ON orders"),
            (Mssql, "USE sales\nDENY SELECT ON orders TO public"),
            (Mssql, "set nocount on\nRECONFIGURE WITH OVERRIDE"),
            (Mssql, "USE sales\nRECEIVE TOP(1) * FROM q"),
            (Mssql, "USE sales WRITETEXT t.c @ptr 'x'"),
            (Mssql, "use sales\nset nocount on"),
            (Mssql, "begin tran\nshutdown"),
            (Mssql, "begin tran t1\nDENY SELECT ON t TO public"),
            (Mssql, "commit transaction\nreconfigure"),
            (Sqlite, "pragma foreign_keys = on"),
            (Mssql, "begin tran\ndrop table t"),
            (Mssql, "commit\ndrop table t"),
            (Mssql, "BEGIN DELETE FROM t END"),
            (Mssql, "begin try\nexec p\nend try"),
            (Oracle, "BEGIN DELETE FROM t; END;"),
            (Oracle, "BEGIN EXECUTE IMMEDIATE 'DROP TABLE t'; END;"),
            (Oracle, "BEGIN pkg.purge_all; END;"),
            (Oracle, "begin null; end;"),
            (Mysql, "START SLAVE"),
            (Mysql, "START REPLICA"),
            (Mysql, "START GROUP_REPLICATION"),
            (Postgres, "end if"),
        ] {
            let c = classify(sql, k);
            assert_eq!(c.kind, StatementKind::Unknown, "{k:?}: {sql}");
            assert!(!crate::sql::read_only_allows(sql, &c), "{k:?}: {sql}");
        }
    }

    #[test]
    fn selects_calling_acting_functions_are_not_reads() {
        for (kind, sql) in [
            (Postgres, "select pg_terminate_backend(123)"),
            (Postgres, "SELECT pg_catalog.pg_cancel_backend(pid) FROM pg_stat_activity WHERE usename = 'app'"),
            (Postgres, "select \"pg_terminate_backend\"(42)"),
            (Postgres, "select count(*) from (select pg_terminate_backend(pid) from pg_stat_activity) x"),
            (Postgres, "with k as (select pg_reload_conf()) select * from k"),
            (Postgres, "select setval('orders_id_seq', 1)"),
            (Postgres, "select nextval('orders_id_seq')"),
            (Postgres, "select lo_export(42, '/tmp/x')"),
            (Postgres, "select * from pg_logical_slot_get_changes('s', null, null)"),
            (Postgres, "select dblink_exec('host=x', 'delete from t')"),
            (Postgres, "select set_config('default_transaction_read_only', 'off', false)"),
            (Snowflake, "select system$cancel_query('01a2')"),
            (Snowflake, "SELECT SYSTEM$ABORT_SESSION(123)"),
            // Any system$ call that isn't a known read.
            (Snowflake, "select system$send_email('int', 'a@b.com', 's', 'body')"),
            (Snowflake, "select system$disable_database_replication('db')"),
            (Snowflake, "select system$global_account_set_parameter('acct', 'ENABLE_X', 'true')"),
            (Snowflake, "select system$some_function_added_next_year()"),
            (Postgres, "select lowrite(lo_open(42, 131072), 'x')"),
            // Sequences: reading the next value advances them.
            (Oracle, "select orders_seq.nextval from dual"),
            (Oracle, "SELECT app.orders_seq.NEXTVAL FROM dual"),
        ] {
            let c = classify(sql, kind);
            assert_eq!(c.kind, StatementKind::Other, "{sql}");
            assert!(!crate::sql::read_only_allows(sql, &c), "{sql}");
        }
        // SQL Server's `NEXT VALUE FOR` (Other, or Unknown where it doesn't parse).
        for sql in [
            "select next value for dbo.orders_seq",
            "SELECT NEXT VALUE FOR orders_seq AS id",
            "select next value for s, x from t",
            "select next -- c\n value for s",
            "select next /* c */ value /* d /* nested */ e */ for s",
            "select next\n\tvalue\r\nfor [dbo].[s]",
            // Comment markers inside strings and quoted names.
            "select '--' as a, next value for s",
            "select [--x], next value for s from t",
            "select '/*' as a, next value for s, '*/' as b",
            "select '--', next /* c */ value for s",
            "select 'it''s -- not', next /* c */ value for s",
        ] {
            let c = classify(sql, Mssql);
            assert!(matches!(c.kind, StatementKind::Other | StatementKind::Unknown), "{sql}: {:?}", c.kind);
            assert!(!crate::sql::read_only_allows(sql, &c), "{sql}");
        }
        // MariaDB has it too, and its block comments don't nest.
        for sql in ["select /* /* */ next value for s", "select next /* x */ value for s"] {
            let c = classify(sql, Mysql);
            assert!(matches!(c.kind, StatementKind::Other | StatementKind::Unknown), "{sql}: {:?}", c.kind);
            assert!(!crate::sql::read_only_allows(sql, &c), "{sql}");
        }
        // Unparsed SQL naming one is asked about.
        assert_eq!(classify("select pg_terminate_backend(1) ))) (((", Postgres).kind, StatementKind::Unknown);
    }

    #[test]
    fn selects_calling_ordinary_functions_stay_reads() {
        for (kind, sql) in [
            (Postgres, "select count(*), lower(name) from users"),
            (Postgres, "select pid, state from pg_stat_activity"),
            (Postgres, "select * from pg_stat_statements"),
            (Postgres, "select currval('orders_id_seq')"),
            (Oracle, "select orders_seq.currval from dual"),
            (Oracle, "select t.nextval_at from t"),
            (Mssql, "select next_value, value from t"),
            (Mssql, "select [next], [value] from t -- for now"),
            (Mssql, "select next_value /* value for */ from t"),
            (Postgres, "select terminate from t"),
            (Postgres, "select pg_terminate_backend from t"),
            (Snowflake, "select system$clustering_information('t')"),
            (Snowflake, "select system$typeof(1)"),
            (Snowflake, "select system$pipe_status('db.s.p')"),
            (Snowflake, "SELECT SYSTEM$STREAM_HAS_DATA('s')"),
            (Snowflake, "select system$estimate_query_acceleration('01a2')"),
            (Snowflake, "select system$get_service_status('svc')"),
        ] {
            assert_eq!(classify(sql, kind).kind, StatementKind::Read, "{sql}");
        }
    }

    /// Several statements as the server reads the text: the worst counts,
    /// though the parser reads a string differently and sees one.
    #[test]
    fn the_worst_statement_counts_where_the_parser_reads_a_string_differently() {
        // DuckDB `e'\''` is a quote (the server); the parser ends the string early.
        for sql in ["select e'\\''; drop table t", "select E'\\'' as q; delete from t where 1=1", "select e'a\\'b'; update t set a = 1 where id = 2"] {
            let k = classify(sql, Duckdb).kind;
            assert!(k.is_write() || k == StatementKind::Unknown, "{sql}: {k:?}");
        }
        // Reads stay reads, one statement or several; one the parser can't
        // read is asked about (never a read).
        for sql in ["select ';' as semi", "select 1; select 'a;b'"] {
            assert_eq!(classify(sql, Duckdb).kind, StatementKind::Read, "{sql}");
        }
        for sql in ["select e'\\''", "select 1; select e'\\''"] {
            assert_eq!(classify(sql, Duckdb).kind, StatementKind::Unknown, "{sql}");
        }
        assert_eq!(classify("select 1; select 2", Postgres).kind, StatementKind::Read);
        assert!(classify("select 1; drop table t", Postgres).kind.is_write());
    }

    #[test]
    fn plain_control_statements_keep_their_kind() {
        for (k, sql) in [
            (Postgres, "begin"),
            (Postgres, "BEGIN ISOLATION LEVEL SERIALIZABLE"),
            (Postgres, "begin transaction read only"),
            (Postgres, "start transaction"),
            (Postgres, "end"),
            (Postgres, "commit"),
            (Postgres, "rollback to savepoint a"),
            (Postgres, "savepoint a"),
            (Postgres, "release savepoint a"),
            (Mysql, "START TRANSACTION WITH CONSISTENT SNAPSHOT"),
            (Mysql, "begin work"),
            (Mssql, "begin tran"),
            (Mssql, "BEGIN DISTRIBUTED TRANSACTION"),
            (Mssql, "commit tran"),
            (Mssql, "begin tran t1"),
            (Mssql, "commit transaction t1"),
            (Sqlite, "begin immediate"),
        ] {
            assert_eq!(classify(sql, k).kind, StatementKind::Transaction, "{k:?}: {sql}");
        }
        for (k, sql) in [(Mssql, "use sales"), (Mssql, "set nocount on"), (Postgres, "set search_path = a, b"), (Mysql, "SET SESSION time_zone = '+00:00'")] {
            let c = classify(sql, k);
            assert_eq!(c.kind, StatementKind::Session, "{k:?}: {sql}");
            assert!(crate::sql::read_only_allows(sql, &c), "{k:?}: {sql}");
        }
    }
}
