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
                k = if keyword == "SELECT" && !contains_dml_word(sql) && !contains_action_word(sql) { StatementKind::Read } else { StatementKind::Unknown };
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
