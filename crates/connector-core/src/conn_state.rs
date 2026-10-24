//! What a session's connection carries beyond single statements.
//!
//! When a result is cut at the row limit, a connection that is still
//! receiving rows can be thrown away and replaced with a fresh one (fast),
//! instead of reading or cancelling the rest of the result (slow, and on
//! some drivers racy). That is only safe when the connection holds nothing
//! the user would lose: no open transaction, no session settings (`SET`,
//! `USE`), no temporary tables, prepared statements or locks. [`ConnState`]
//! follows the statements run on a session to tell.
//!
//! It is a heuristic over statement text and errs towards "stateful": then
//! the connection is kept and the rest of the result is read (correct, only
//! slower). State created inside procedures (`EXEC`, `CALL`) is not seen;
//! drivers that report an open transaction should check that too.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::sql::{leading_keyword, words};
use crate::types::ConnectorKind;

#[derive(Debug, Default)]
pub struct ConnState {
    /// Settings, temporary objects… (kept until the connection is replaced).
    settings: AtomicBool,
    /// Open transactions (nesting counted for SQL Server).
    tx_depth: AtomicU32,
}

impl ConnState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note a statement about to run on the connection.
    pub fn observe(&self, sql: &str, kind: ConnectorKind) {
        let kw = leading_keyword(sql);
        let has = |w: &[&str]| words(sql).any(|x| w.iter().any(|k| x.eq_ignore_ascii_case(k)));
        let mssql = kind == ConnectorKind::Mssql;
        // Transactions.
        let begins = match kw.as_str() {
            "START" => has(&["TRANSACTION"]),
            // SQL Server: BEGIN TRAN; BEGIN … END / BEGIN TRY blocks only when they mention one.
            "BEGIN" if mssql => has(&["TRAN", "TRANSACTION"]),
            "BEGIN" => true,
            _ => mssql && has(&["TRAN", "TRANSACTION"]) && !matches!(kw.as_str(), "COMMIT" | "ROLLBACK"),
        };
        if begins {
            self.tx_depth.fetch_add(1, Ordering::SeqCst);
        }
        match kw.as_str() {
            // ROLLBACK TO SAVEPOINT keeps the transaction.
            "ROLLBACK" if has(&["TO"]) => {}
            "COMMIT" | "END" if mssql => {
                let _ = self.tx_depth.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |d| Some(d.saturating_sub(1)));
            }
            "COMMIT" | "ROLLBACK" | "END" | "ABORT" => self.tx_depth.store(0, Ordering::SeqCst),
            _ => {}
        }
        // Session state.
        let session_kw = matches!(
            kw.as_str(),
            "SET" | "USE" | "RESET" | "DISCARD" | "PREPARE" | "DECLARE" | "LOCK" | "LISTEN" | "SAVEPOINT" | "XA" | "ATTACH" | "PRAGMA"
        ) && !(mssql && kw == "DECLARE"); // T-SQL variables live for one batch
        let temp = has(&["TEMP", "TEMPORARY", "SET_CONFIG", "PG_ADVISORY_LOCK", "GET_LOCK", "SP_SET_SESSION_CONTEXT", "IMPLICIT_TRANSACTIONS"])
            || (mssql && sql.contains('#'))
            || (kind == ConnectorKind::Mysql && sql.contains(":="));
        if session_kw || temp {
            self.settings.store(true, Ordering::SeqCst);
        }
    }

    /// A transaction is (probably) open.
    pub fn in_transaction(&self) -> bool {
        self.tx_depth.load(Ordering::SeqCst) > 0
    }

    /// The connection can be replaced without losing anything.
    pub fn replaceable(&self) -> bool {
        !self.in_transaction() && !self.settings.load(Ordering::SeqCst)
    }

    /// A new connection replaced the old one: nothing is carried any more.
    pub fn reset(&self) {
        self.settings.store(false, Ordering::SeqCst);
        self.tx_depth.store(0, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectorKind::*;

    fn after(kind: ConnectorKind, stmts: &[&str]) -> (bool, bool) {
        let s = ConnState::new();
        for q in stmts {
            s.observe(q, kind);
        }
        (s.replaceable(), s.in_transaction())
    }

    #[test]
    fn reads_and_writes_keep_the_connection_replaceable() {
        for k in [Postgres, Mysql, Mssql] {
            assert_eq!(after(k, &["select * from t", "update t set a = 1 where id = 2", "insert into t values (1)", "with x as (select 1) select * from x"]), (true, false), "{k:?}");
        }
        assert_eq!(after(Mssql, &["declare @n int = 5; select top (@n) * from t"]), (true, false));
    }

    #[test]
    fn transactions() {
        assert_eq!(after(Postgres, &["begin", "select 1"]), (false, true));
        assert_eq!(after(Postgres, &["begin", "commit"]), (true, false));
        assert_eq!(after(Mysql, &["start transaction", "rollback to savepoint a"]).1, true);
        assert_eq!(after(Mysql, &["start transaction", "rollback"]), (true, false));
        assert_eq!(after(Mssql, &["begin tran", "begin transaction", "commit"]).1, true, "nested");
        assert_eq!(after(Mssql, &["begin tran", "begin transaction", "commit", "commit tran"]), (true, false));
        assert_eq!(after(Mssql, &["begin try select 1 end try begin catch end catch"]), (true, false));
    }

    #[test]
    fn session_state_sticks() {
        assert!(!after(Postgres, &["set search_path = x"]).0);
        assert!(!after(Postgres, &["create temp table t (a int)"]).0);
        assert!(!after(Mysql, &["use other"]).0);
        assert!(!after(Mysql, &["select @a := 1"]).0);
        assert!(!after(Mssql, &["select * into #t from x"]).0);
        assert!(!after(Mssql, &["set implicit_transactions on"]).0);
        assert!(!after(Postgres, &["set search_path = x", "commit"]).0, "settings outlive transactions");
    }
}
