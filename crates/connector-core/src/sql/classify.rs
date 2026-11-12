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
            if out.kind < StatementKind::Dml && (stmts.iter().any(calls_acting_function) || next_value_for(sql)) {
                out.kind = StatementKind::Other;
            }
            out
        }
        _ => {
            let mut k = keyword_kind(&keyword);
            if k == StatementKind::Read && keyword == "WITH" && contains_dml_word(sql) {
                k = StatementKind::Dml;
            }
            if k == StatementKind::Read {
                // Unparseable but looks like a read: a read only when no word
                // in it can act (T-SQL runs `SELECT 1 EXEC xp_cmdshell …` as
                // two statements without a separator).
                k = if keyword == "SELECT" && !contains_dml_word(sql) && !contains_action_word(sql) && !names_acting_function(sql) {
                    StatementKind::Read
                } else {
                    StatementKind::Unknown
                };
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

/// Unparsed SQL naming an acting function (`$` kept for Snowflake's).
fn names_acting_function(sql: &str) -> bool {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$')).any(is_acting_function) || next_value_for(sql)
}

/// SQL Server / standard `NEXT VALUE FOR seq`, which advances the sequence
/// (sqlparser reads it as columns and aliases). Also inside a string: a
/// read is never claimed when unsure.
fn next_value_for(sql: &str) -> bool {
    let w: Vec<&str> = words(sql).collect();
    w.windows(3).any(|w| w[0].eq_ignore_ascii_case("next") && w[1].eq_ignore_ascii_case("value") && w[2].eq_ignore_ascii_case("for"))
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
        for sql in ["select next value for dbo.orders_seq", "SELECT NEXT VALUE FOR orders_seq AS id", "select next value for s, x from t"] {
            let c = classify(sql, Mssql);
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
