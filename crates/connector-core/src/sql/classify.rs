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
}
