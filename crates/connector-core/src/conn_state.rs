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

use crate::TxState;
use crate::sql::{leading_keyword, words};
use crate::types::ConnectorKind;

#[derive(Debug, Default)]
pub struct ConnState {
    /// Settings, temporary objects… (kept until the connection is replaced).
    settings: AtomicBool,
    /// Open transactions (nesting counted for SQL Server).
    tx_depth: AtomicU32,
    /// Writes open a transaction by themselves (SQL Server
    /// `IMPLICIT_TRANSACTIONS ON`, autocommit off): one may be open at any
    /// time, which statement text can't follow.
    implicit: AtomicBool,
}

impl ConnState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note a statement about to run on the connection.
    pub fn observe(&self, sql: &str, kind: ConnectorKind) {
        let has = |w: &[&str]| words(sql).any(|x| w.iter().any(|k| x.eq_ignore_ascii_case(k)));
        let mssql = kind == ConnectorKind::Mssql;
        // The statement's words without comments and string contents, upper
        // case: `ROLLBACK -- undo` is a plain ROLLBACK, `[Transaction]` in a
        // SELECT is a table name, not a BEGIN.
        let code = code_words(sql, kind);
        // The first word of code: after `//` or `#` comments too.
        let mut kw = code.first().cloned().unwrap_or_else(|| leading_keyword(sql));
        // SQL Server: a guarded `IF @@TRANCOUNT > 0 ROLLBACK|COMMIT …` ends
        // as the plain form does; read from that word on.
        let mut tail: &[String] = &code;
        if mssql
            && kw == "IF"
            && let Some(i) = code.iter().position(|w| w == "ROLLBACK" || w == "COMMIT")
        {
            kw = code[i].clone();
            tail = &code[i..];
        }
        let at = |i: usize| tail.get(i).map(String::as_str);
        let tran_word = |w: Option<&str>| matches!(w, Some("TRAN" | "TRANSACTION"));
        // `ROLLBACK TO [SAVEPOINT] x`, `ROLLBACK WORK TO …`.
        let to_savepoint = at(1) == Some("TO") || (matches!(at(1), Some("TRAN" | "TRANSACTION" | "WORK")) && at(2) == Some("TO"));
        // SQL Server: `SAVE TRAN x` and `ROLLBACK TRAN x` (to a savepoint,
        // the name right after TRAN; plain `ROLLBACK [WORK]` can't name one)
        // leave the transaction as it is.
        let keeps = mssql && (kw == "SAVE" || (kw == "ROLLBACK" && tran_word(at(1)) && at(2).is_some_and(|w| !STATEMENT_WORDS.contains(&w))));
        // SQL Server: BEGIN TRAN / BEGIN DISTRIBUTED TRAN anywhere in the
        // batch (`SET XACT_ABORT ON; BEGIN TRAN`, `IF … BEGIN TRANSACTION`).
        let mssql_begin = || code.windows(2).any(|w| w[0] == "BEGIN" && matches!(w[1].as_str(), "TRAN" | "TRANSACTION" | "DISTRIBUTED"));
        // Transactions.
        let begins = match kw.as_str() {
            "START" => has(&["TRANSACTION"]),
            "BEGIN" if mssql => mssql_begin(),
            // Not a block (`BEGIN … END;`, Snowflake Scripting): only
            // `BEGIN`, `BEGIN;` and BEGIN followed by transaction words.
            "BEGIN" => {
                matches!(
                    code.get(1).map(String::as_str),
                    None | Some(
                        "TRANSACTION" | "TRAN" | "WORK" | "NAME" | "ISOLATION" | "READ" | "NOT" | "DEFERRABLE" | "DEFERRED" | "IMMEDIATE" | "EXCLUSIVE"
                    )
                )
            }
            // Also after a COMMIT or ROLLBACK in the same batch (`commit
            // begin tran`): counted after the end below.
            _ => mssql && !keeps && mssql_begin(),
        };
        match kw.as_str() {
            // ROLLBACK TO SAVEPOINT keeps the transaction.
            "ROLLBACK" if to_savepoint || keeps => {}
            // Postgres `COMMIT PREPARED 'x'` / `ROLLBACK PREPARED 'x'` end a
            // two-phase transaction, not this session's: inside one they
            // fail and leave it open (aborted, waiting for ROLLBACK).
            "COMMIT" | "ROLLBACK" if at(1) == Some("PREPARED") => {}
            // Postgres `PREPARE TRANSACTION 'x'`: the transaction leaves the
            // session (for a later COMMIT PREPARED): none is open here.
            "PREPARE" if at(1) == Some("TRANSACTION") => self.tx_depth.store(0, Ordering::SeqCst),
            "COMMIT" | "END" if mssql => {
                // `try_update` is the new name but needs Rust 1.95; the MSRV is 1.89.
                #[allow(deprecated)]
                let _ = self.tx_depth.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |d| Some(d.saturating_sub(1)));
            }
            "COMMIT" | "ROLLBACK" | "END" | "ABORT" => self.tx_depth.store(0, Ordering::SeqCst),
            _ => {}
        }
        if begins {
            self.tx_depth.fetch_add(1, Ordering::SeqCst);
        }
        // `COMMIT AND CHAIN` / `ROLLBACK AND CHAIN` start the next one at once.
        if matches!(kw.as_str(), "COMMIT" | "ROLLBACK") && code.windows(2).any(|w| w[0] == "AND" && w[1] == "CHAIN") {
            let _ = self.tx_depth.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        }
        // Implicit transactions on or off: SQL Server's
        // `IMPLICIT_TRANSACTIONS` (also through `ANSI_DEFAULTS`), autocommit
        // elsewhere. Read only from code (not comments or strings), as a
        // setting: `SET x ON`, `SET SESSION x = 0`, `ALTER SESSION SET x =
        // 'false'`. A value may be quoted.
        if let Some(on) = implicit_setting(sql, kind) {
            self.implicit.store(on, Ordering::SeqCst);
        }
        // While writes open a transaction by themselves, a write did:
        // one is open until COMMIT or ROLLBACK (Commit and Rollback show).
        // SQL Server's DDL too (Snowflake's commits).
        let write = matches!(kw.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE")
            || (mssql && matches!(kw.as_str(), "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "GRANT" | "REVOKE"));
        if write && self.implicit.load(Ordering::SeqCst) {
            let _ = self.tx_depth.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        }
        // Snowflake's session parameters (`ALTER SESSION SET …`).
        if kw == "ALTER" && code.get(1).map(String::as_str) == Some("SESSION") {
            self.settings.store(true, Ordering::SeqCst);
        }
        // Session state.
        let session_kw =
            matches!(kw.as_str(), "SET" | "USE" | "RESET" | "DISCARD" | "PREPARE" | "DECLARE" | "LOCK" | "LISTEN" | "SAVEPOINT" | "XA" | "ATTACH" | "PRAGMA")
                && !(mssql && kw == "DECLARE"); // T-SQL variables live for one batch
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

    /// What a session reports: `Unknown` while writes open transactions by
    /// themselves and none is known to be open (a read may have opened one
    /// on SQL Server): the session is kept (idle close, the row-limit
    /// replace), as if one were. A write in that mode opened one: `Open`,
    /// so the UI's Commit and Rollback show (they show for `Open` only).
    pub fn tx_state(&self) -> TxState {
        if self.in_transaction() {
            TxState::Open
        } else if self.implicit.load(Ordering::SeqCst) {
            TxState::Unknown
        } else {
            TxState::None
        }
    }

    /// The connection can be replaced without losing anything.
    pub fn replaceable(&self) -> bool {
        self.tx_state() == TxState::None && !self.settings.load(Ordering::SeqCst)
    }

    /// The open transactions as the server counts them (SQL Server's
    /// `@@TRANCOUNT`): replaces what the statements' text suggested.
    pub fn set_depth(&self, depth: u32) {
        self.tx_depth.store(depth, Ordering::SeqCst);
    }

    /// Whether this batch may have changed the transaction depth so that
    /// asking the server is worth a round trip: it names transaction control
    /// (`BEGIN TRAN`, `COMMIT`, `ROLLBACK`, `SAVE`, `XACT_ABORT`), or writes
    /// open transactions by themselves (implicit transactions), or it failed
    /// inside one (a deadlock or `XACT_ABORT` ends it).
    pub fn may_change_depth(&self, sql: &str, failed: bool) -> bool {
        let w = |k: &str| words(sql).any(|x| x.eq_ignore_ascii_case(k));
        ["BEGIN", "COMMIT", "ROLLBACK", "SAVE", "TRAN", "TRANSACTION", "XACT_ABORT", "IMPLICIT_TRANSACTIONS", "ANSI_DEFAULTS"].iter().any(|k| w(k))
            || self.implicit.load(Ordering::SeqCst)
            || (failed && self.in_transaction())
    }

    /// A new connection replaced the old one: nothing is carried any more.
    pub fn reset(&self) {
        self.settings.store(false, Ordering::SeqCst);
        self.tx_depth.store(0, Ordering::SeqCst);
        self.implicit.store(false, Ordering::SeqCst);
    }
}

/// Words that start a statement: after `ROLLBACK TRAN` one of them is the
/// next statement of the batch, not a savepoint name.
const STATEMENT_WORDS: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "WITH",
    "PRINT",
    "IF",
    "BEGIN",
    "END",
    "SET",
    "EXEC",
    "EXECUTE",
    "DECLARE",
    "COMMIT",
    "ROLLBACK",
    "SAVE",
    "WHILE",
    "RETURN",
    "USE",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "RAISERROR",
    "THROW",
    "GO",
];

/// Upper-cased words of `sql` outside comments and quoted strings.
fn code_words(sql: &str, kind: ConnectorKind) -> Vec<String> {
    words(&crate::sql::code_text(sql, kind)).map(str::to_ascii_uppercase).collect()
}

/// SQL Server options that can share one `SET a, b, c ON`.
const MSSQL_SET_OPTIONS: &[&str] = &[
    "ANSI_DEFAULTS",
    "ANSI_NULLS",
    "ANSI_NULL_DFLT_OFF",
    "ANSI_NULL_DFLT_ON",
    "ANSI_PADDING",
    "ANSI_WARNINGS",
    "ARITHABORT",
    "ARITHIGNORE",
    "CONCAT_NULL_YIELDS_NULL",
    "CURSOR_CLOSE_ON_COMMIT",
    "IMPLICIT_TRANSACTIONS",
    "NOCOUNT",
    "NUMERIC_ROUNDABORT",
    "QUOTED_IDENTIFIER",
    "XACT_ABORT",
];

/// Whether this batch turns implicit transactions on (`Some(true)`: writes
/// open a transaction by themselves) or off, read from its settings only:
/// SQL Server `SET IMPLICIT_TRANSACTIONS ON` (also `ANSI_DEFAULTS`, and in
/// a list `SET ANSI_NULLS, IMPLICIT_TRANSACTIONS ON`), Snowflake `ALTER
/// SESSION SET AUTOCOMMIT = FALSE` / `UNSET AUTOCOMMIT`, MySQL `SET
/// [SESSION] autocommit = 0`. Comments, strings and other statements that
/// name the word (`where note = 'autocommit off'`, `update t set
/// implicit_transactions = 1`) don't count. The last setting wins.
fn implicit_setting(sql: &str, kind: ConnectorKind) -> Option<bool> {
    let mssql = kind == ConnectorKind::Mssql;
    let code = crate::sql::code_text_values(sql, kind);
    let mut result = None;
    // `Some(true)` for ON, `Some(false)` for OFF, after an optional `TO`.
    let value = |w: &[String], i: usize| -> Option<bool> {
        let v = w.get(i).filter(|v| *v != "TO").or_else(|| w.get(i + 1))?;
        match v.as_str() {
            "ON" | "TRUE" | "1" => Some(true),
            "OFF" | "FALSE" | "0" => Some(false),
            _ => None,
        }
    };
    for stmt in code.split(';') {
        let w: Vec<String> = words(stmt).map(str::to_ascii_uppercase).collect();
        let at = |i: usize| w.get(i).map(String::as_str);
        if at(0) == Some("ALTER") && at(1) == Some("SESSION") {
            match at(2) {
                // Back to the default: autocommit on.
                Some("UNSET") if w.iter().any(|x| x == "AUTOCOMMIT") => result = Some(false),
                // Snowflake: `SET a = 1 b = 2`, one statement.
                Some("SET") => {
                    for i in 3..w.len() {
                        if w[i] == "AUTOCOMMIT"
                            && let Some(on) = value(&w, i + 1)
                        {
                            result = Some(!on);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        // Snowflake's `SET x = …` sets a variable.
        if kind == ConnectorKind::Snowflake {
            continue;
        }
        for j in (0..w.len()).filter(|&j| w[j] == "SET" && starts_statement(&w, j, mssql)) {
            let mut i = j + 1;
            if mssql {
                let mut implicit = false;
                while let Some(opt) = at(i).filter(|o| MSSQL_SET_OPTIONS.contains(o)) {
                    implicit |= matches!(opt, "IMPLICIT_TRANSACTIONS" | "ANSI_DEFAULTS");
                    i += 1;
                }
                if implicit && let Some(on) = value(&w, i) {
                    result = Some(on);
                }
            } else {
                // `SET SESSION autocommit`, `SET @@session.autocommit`, `SET LOCAL …`.
                while matches!(at(i), Some("SESSION" | "LOCAL")) {
                    i += 1;
                }
                if at(i) == Some("AUTOCOMMIT")
                    && let Some(on) = value(&w, i + 1)
                {
                    result = Some(!on);
                }
            }
        }
    }
    result
}

/// Whether the `SET` at `w[j]` starts a statement. Elsewhere statements
/// are split at `;`; a T-SQL batch needs none, so there a `SET` starts one
/// unless it belongs to an `UPDATE` (or a `MERGE`'s `UPDATE SET`).
fn starts_statement(w: &[String], j: usize, mssql: bool) -> bool {
    if j == 0 {
        return true;
    }
    mssql && w[..j].iter().rev().find(|x| STATEMENT_WORDS.contains(&x.as_str())).is_none_or(|x| x != "UPDATE" && x != "MERGE")
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
            assert_eq!(
                after(k, &["select * from t", "update t set a = 1 where id = 2", "insert into t values (1)", "with x as (select 1) select * from x"]),
                (true, false),
                "{k:?}"
            );
        }
        assert_eq!(after(Mssql, &["declare @n int = 5; select top (@n) * from t"]), (true, false));
    }

    #[test]
    fn transactions() {
        assert_eq!(after(Postgres, &["begin", "select 1"]), (false, true));
        assert_eq!(after(Postgres, &["begin", "commit"]), (true, false));
        assert!(after(Mysql, &["start transaction", "rollback to savepoint a"]).1);
        assert_eq!(after(Mysql, &["start transaction", "rollback"]), (true, false));
        assert!(after(Mssql, &["begin tran", "begin transaction", "commit"]).1, "nested");
        assert_eq!(after(Mssql, &["begin tran", "begin transaction", "commit", "commit tran"]), (true, false));
        assert_eq!(after(Mssql, &["begin try select 1 end try begin catch end catch"]), (true, false));
        // Snowflake follows its transactions this way (its API doesn't report them).
        assert!(after(Snowflake, &["begin", "insert into t values (1)"]).1);
        assert!(after(Snowflake, &["begin transaction", "select 1"]).1);
        assert!(!after(Snowflake, &["begin", "commit"]).1);
        assert!(!after(Snowflake, &["start transaction", "rollback"]).1);
        // A Snowflake Scripting block is not a transaction.
        assert!(!after(Snowflake, &["BEGIN\n  LET x := 1;\n  RETURN x;\nEND"]).1);
        assert!(!after(Snowflake, &["begin\n  insert into t values (1);\nend"]).1);
        // The statements DataBrain sends itself (TxDialect): the guarded
        // rollback and savepoints follow the server's @@TRANCOUNT.
        let tx = Mssql.traits().transactions.unwrap();
        let (save, back) = tx.savepoint.unwrap();
        let (save, back) = (save.replace("{}", "databrain_apply"), back.replace("{}", "databrain_apply"));
        assert!(!after(Mssql, &[tx.rollback]).1, "a guarded rollback on no transaction opens none");
        assert!(!after(Mssql, &[tx.begin, tx.rollback]).1);
        assert!(!after(Mssql, &[tx.begin, &save, tx.commit]).1, "a savepoint isn't a level");
        assert!(after(Mssql, &[tx.begin, &save, &back]).1, "back to the savepoint: still open");
        assert!(!after(Mssql, &[tx.begin, &save, &back, tx.commit]).1);
        assert!(!after(Mssql, &["begin tran", "IF @@TRANCOUNT > 0 COMMIT"]).1);
        assert!(after(Mssql, &["begin tran", "rollback tran sp1"]).1);
        assert!(!after(Mssql, &["begin tran", "rollback tran"]).1);
        // A comment or the next statement after ROLLBACK is not a savepoint.
        for back in ["rollback -- undo it", "ROLLBACK TRANSACTION /* all */", "rollback\nselect @@trancount", "rollback tran\nprint 'done'"] {
            assert!(!after(Mssql, &["begin tran", back]).1, "{back}");
        }
        assert!(!after(Postgres, &["begin", "rollback -- back to start"]).1);
        assert!(after(Postgres, &["begin", "rollback work to savepoint a"]).1);
        // Naming a table Transaction doesn't begin one; a batch that does, does.
        assert!(!after(Mssql, &["select * from dbo.[Transaction]"]).1);
        assert!(!after(Mssql, &["begin try select * from [Transaction] end try begin catch end catch"]).1);
        assert!(!after(Mssql, &["select 'begin tran'"]).1);
        assert!(after(Mssql, &["set xact_abort on; begin tran"]).1);
        assert!(after(Mssql, &["begin distributed transaction"]).1);
        // Chained: the next transaction starts at once.
        assert!(after(Postgres, &["begin", "commit and chain", "insert into t values (1)"]).1);
        assert!(after(Postgres, &["begin", "rollback and chain"]).1);
        assert!(!after(Postgres, &["begin", "commit and no chain"]).1);
        // A batch that ends one and begins the next.
        assert!(after(Mssql, &["commit\nbegin tran"]).1);
        assert!(after(Mssql, &["begin tran", "if @@trancount > 0 commit\nbegin tran"]).1);
        assert!(!after(Mssql, &["select * into #tran from t"]).1, "a temp table named tran");
        // Quoted names with a quote or comment marker inside don't hide a BEGIN.
        for batch in [
            "select [O'Brien] from t\nbegin tran\nupdate t set a = 'x'",
            "select [a--b] from t\nbegin tran\nupdate t set a = 1",
            "select \"a--b\" from t; begin tran",
        ] {
            assert!(after(Mssql, &[batch]).1, "{batch}");
        }
        // Comments the dialect has before the first word.
        assert!(after(Snowflake, &["// c\nbegin"]).1);
        assert!(after(Mysql, &["# c\nbegin"]).1);
        assert!(after(Postgres, &["begin isolation level serializable"]).1);
        assert!(after(Sqlite, &["begin immediate"]).1);
    }

    #[test]
    fn prepared_commits_leave_the_sessions_transaction_alone() {
        let state = |stmts: &[&str]| {
            let s = ConnState::new();
            for q in stmts {
                s.observe(q, Postgres);
            }
            s.tx_state()
        };
        assert_eq!(state(&["begin", "commit prepared 'tx1'"]), TxState::Open, "fails inside a transaction: still open");
        assert_eq!(state(&["begin", "rollback prepared 'tx1'"]), TxState::Open);
        assert_eq!(state(&["commit prepared 'tx1'"]), TxState::None);
        assert_eq!(state(&["begin", "commit"]), TxState::None, "a plain COMMIT ends it");
        assert_eq!(state(&["begin", "prepare transaction 'tx1'"]), TxState::None, "PREPARE TRANSACTION hands it over");
    }

    /// The server's count replaces what the text suggested (SQL Server's
    /// `@@TRANCOUNT`); which batches are worth asking about.
    #[test]
    fn the_servers_depth_wins() {
        let s = ConnState::new();
        s.observe("begin tran; insert into t values (1); commit", Mssql);
        assert_eq!(s.tx_state(), TxState::Open, "the text: conservative");
        s.set_depth(0);
        assert_eq!(s.tx_state(), TxState::None, "the server: committed");
        assert!(s.may_change_depth("if @@trancount > 0 commit tran", false));
        assert!(!s.may_change_depth("select * from t", false), "a plain read: no round trip");
        assert!(!s.may_change_depth("select * from t", true), "failed outside a transaction");
        s.set_depth(1);
        assert!(s.may_change_depth("update t set a = 1", true), "failed inside one (deadlock, XACT_ABORT)");
        let i = ConnState::new();
        i.observe("set implicit_transactions on", Mssql);
        assert!(i.may_change_depth("select * from t", false), "implicit: a read may open one");
    }

    /// Writes that open a transaction by themselves: one may be open at
    /// any time, so the session says it can't tell (and is kept).
    #[test]
    fn implicit_transactions() {
        let state = |kind, stmts: &[&str]| {
            let s = ConnState::new();
            for q in stmts {
                s.observe(q, kind);
            }
            (s.tx_state(), s.replaceable())
        };
        // On, before a write: one may be open (a SELECT opens one on SQL Server).
        assert_eq!(state(Mssql, &["set implicit_transactions on"]), (TxState::Unknown, false));
        assert_eq!(state(Mssql, &["set implicit_transactions on", "select * from t"]).0, TxState::Unknown);
        // A write opened one: open until it ends.
        assert_eq!(state(Mssql, &["set implicit_transactions on", "insert into t values (1)"]), (TxState::Open, false));
        assert_eq!(state(Mssql, &["set implicit_transactions on", "update t set a = 1", "commit"]).0, TxState::Unknown);
        assert_eq!(state(Mssql, &["set implicit_transactions on", "create table x (a int)", "rollback"]).0, TxState::Unknown);
        assert_eq!(state(Snowflake, &["alter session set autocommit = false", "delete from t where a = 1"]).0, TxState::Open);
        assert_eq!(state(Snowflake, &["alter session set autocommit = false", "create table x (a int)"]).0, TxState::Unknown, "Snowflake DDL commits");
        // Off: writes commit by themselves.
        assert_eq!(state(Mssql, &["insert into t values (1)"]).0, TxState::None);
        assert_eq!(state(Snowflake, &["alter session set autocommit = true", "insert into t values (1)"]).0, TxState::None);
        // ANSI_DEFAULTS turns it on and off on SQL Server; quoted values count.
        assert_eq!(state(Mssql, &["set ansi_defaults on", "insert into t values (1)"]).0, TxState::Open);
        assert_eq!(state(Mssql, &["set ansi_defaults on", "set ansi_defaults off", "insert into t values (1)"]).0, TxState::None);
        assert_eq!(state(Snowflake, &["alter session set autocommit = 'false'"]).0, TxState::Unknown);
        assert_eq!(state(Snowflake, &["alter session set autocommit = 'false'", "alter session set autocommit = 'true'"]).0, TxState::None);
        // The word in a query isn't a setting.
        assert_eq!(state(Snowflake, &["select autocommit from settings"]).0, TxState::None);
        assert_eq!(state(Mssql, &["set implicit_transactions on", "set implicit_transactions off"]).0, TxState::None);
        assert_eq!(state(Snowflake, &["alter session set autocommit = false", "insert into t values (1)"]), (TxState::Open, false));
        assert_eq!(state(Snowflake, &["alter session set autocommit = false", "alter session set autocommit = true"]).0, TxState::None);
        assert!(!state(Snowflake, &["alter session set timezone = 'UTC'"]).1, "a session parameter is session state");
        assert_eq!(state(Mssql, &["set implicit_transactions on", "begin tran"]).0, TxState::Open);
    }

    /// Only a setting turns implicit transactions on: the words in a
    /// string, a comment, a column or a variable don't, so a write after
    /// them doesn't look like it opened a transaction (manual commit then
    /// sends BEGIN, and Rollback undoes it).
    #[test]
    fn implicit_transactions_from_settings_only() {
        let state = |kind, stmts: &[&str]| {
            let s = ConnState::new();
            for q in stmts {
                s.observe(q, kind);
            }
            s.tx_state()
        };
        let write = "update audit set seen = 1 where id = 1";
        for (kind, signal) in [
            (Snowflake, "select * from audit where note = 'autocommit off'"),
            (Snowflake, "select 1 -- alter session set autocommit = false"),
            (Snowflake, "set autocommit = false"),
            (Snowflake, "select autocommit, false from settings"),
            (Mssql, "select * from audit where note = 'set implicit_transactions on'"),
            (Mssql, "update t set implicit_transactions = 1"),
            (Mssql, "/* set implicit_transactions on */ select 1"),
            (Mssql, "merge t using s on t.id = s.id when matched then update set ansi_defaults = 1;"),
            (Mysql, "select * from audit where note = 'autocommit = 0'"),
            (Mysql, "update t set autocommit = 0"),
        ] {
            assert_eq!(state(kind, &[signal]), TxState::None, "{kind:?}: {signal}");
            assert_eq!(state(kind, &[signal, write]), TxState::None, "{kind:?}: {signal}");
        }
        // Settings do, in each form.
        for (kind, setting) in [
            (Snowflake, "alter session set autocommit = false"),
            (Snowflake, "ALTER SESSION SET TIMEZONE = 'UTC' AUTOCOMMIT = 'FALSE'"),
            (Mssql, "set implicit_transactions on"),
            (Mssql, "set nocount on\nset implicit_transactions on"),
            (Mssql, "set ansi_nulls, implicit_transactions on"),
            (Mssql, "declare @a int set @a = 1 set implicit_transactions on"),
            (Mssql, "-- turn it on\nSET IMPLICIT_TRANSACTIONS ON"),
            (Mysql, "set autocommit = 0"),
            (Mysql, "set session autocommit = 'OFF'"),
            (Mysql, "select 1; set @@session.autocommit = 0"),
        ] {
            assert_eq!(state(kind, &[setting]), TxState::Unknown, "{kind:?}: {setting}");
            assert_eq!(state(kind, &[setting, write]), TxState::Open, "{kind:?}: {setting}");
        }
        assert_eq!(state(Snowflake, &["alter session set autocommit = false", "alter session unset autocommit"]), TxState::None);
        assert_eq!(state(Mssql, &["set implicit_transactions on; set implicit_transactions off"]), TxState::None, "the last wins");
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
