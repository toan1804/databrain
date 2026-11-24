//! Statement execution on both drivers: streaming, cancellation and blocking calls.

use crate::agent;
use crate::error::map_err;
use crate::types::col_type;
use crate::types::convert;
use crate::types::db_type;
use databrain_connector_core::BatchBuilder;
use databrain_connector_core::ColType;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ExecOptions;
use databrain_connector_core::ExecSummary;
use databrain_connector_core::QueryStream;
use databrain_connector_core::Result;
use databrain_connector_core::StreamEvent;
use databrain_connector_core::StreamSender;
use databrain_connector_core::value::Column;
use oracle::Connection;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[derive(Default)]
pub(crate) struct CallState {
    /// The caller stopped waiting.
    pub(crate) abandoned: AtomicBool,
    /// The statement holds the connection now.
    pub(crate) running: AtomicBool,
}

pub(crate) struct BreakOnDrop {
    pub(crate) conn: Arc<Connection>,
    pub(crate) state: Arc<CallState>,
    pub(crate) armed: bool,
}

impl Drop for BreakOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.state.abandoned.store(true, Ordering::SeqCst);
        if self.state.running.load(Ordering::SeqCst) {
            let _ = self.conn.break_execution();
        }
    }
}

pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ConnectorError::internal(e.to_string()))?
}

/// SQL as Oracle accepts it (no trailing `;`, except for PL/SQL blocks) and
/// whether it returns rows.
pub(crate) fn prepare_sql(sql: &str) -> (&str, bool) {
    let trimmed = sql.trim_end();
    let kw = databrain_connector_core::sql::leading_keyword(trimmed);
    let sql = if !is_plsql(trimmed) { trimmed.trim_end_matches(';') } else { trimmed };
    (sql, kw == "SELECT" || kw == "WITH")
}

/// A PL/SQL block or unit, which keeps its final `;`: an anonymous block, or
/// `CREATE [OR REPLACE] [EDITIONABLE | NONEDITIONABLE] PROCEDURE | FUNCTION |
/// PACKAGE | TRIGGER | TYPE | LIBRARY` (decided by what is created, not by
/// an `END` somewhere: `end;` may start a line, and a view's `CASE … END`
/// is plain SQL).
pub(crate) fn is_plsql(sql: &str) -> bool {
    let mut w = head_words(sql, 6).into_iter();
    match w.next().as_deref() {
        Some("BEGIN" | "DECLARE") => return true,
        Some("CREATE") => {}
        _ => return false,
    }
    let mut next = w.next();
    if next.as_deref() == Some("OR") {
        w.next(); // REPLACE
        next = w.next();
    }
    if matches!(next.as_deref(), Some("EDITIONABLE" | "NONEDITIONABLE")) {
        next = w.next();
    }
    matches!(next.as_deref(), Some("PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY"))
}

/// Whether DBMS_OUTPUT is read after `sql`: PL/SQL blocks and `CALL`, where
/// `put_line` output comes from (one extra round trip, not after every query).
pub(crate) fn reads_output(sql: &str) -> bool {
    let w = head_words(sql, 1);
    matches!(w.first().map(String::as_str), Some("BEGIN" | "DECLARE" | "CALL"))
}

/// The first `n` words of `sql` in upper case, comments skipped (a quoted
/// name is an empty word: never a keyword).
fn head_words(sql: &str, n: usize) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() && out.len() < n {
        if b[i..].starts_with(b"--") {
            i = sql[i..].find('\n').map_or(b.len(), |k| i + k + 1);
        } else if b[i..].starts_with(b"/*") {
            i = sql[i + 2..].find("*/").map_or(b.len(), |k| i + 2 + k + 2);
        } else if b[i].is_ascii_alphanumeric() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'$' | b'#')) {
                i += 1;
            }
            out.push(sql[start..i].to_ascii_uppercase());
        } else if b[i] == b'"' {
            i = sql[i + 1..].find('"').map_or(b.len(), |k| i + 1 + k + 1);
            out.push(String::new());
        } else {
            i += 1;
        }
    }
    out
}

/// Lines of DBMS_OUTPUT read after a statement at most (and their bytes):
/// a loop printing millions of lines doesn't fill memory.
pub(crate) const OUTPUT_MAX_LINES: usize = 10_000;
pub(crate) const OUTPUT_MAX_BYTES: usize = 1 << 20;

/// Read DBMS_OUTPUT lines on the Instant Client driver, up to 32767 bytes
/// per call (a call that fails loses the lines it read): `:1` the lines
/// (each ended by a line break), `:2` 1 when none are left, `:3` a line that
/// didn't fit in `:1` (taken from the buffer, so returned on its own). A
/// line is at most 32767 bytes, so none is cut. The thin driver's helper
/// reads them into a CLOB instead (`outputSQL` in agent/main.go: its string
/// out-binds hold about 8 KB).
pub(crate) const OUTPUT_SQL: &str = "declare l varchar2(32767); s integer := 0; b varchar2(32767); c varchar2(32767); \
     begin loop dbms_output.get_line(l, s); exit when s <> 0; \
     if nvl(lengthb(b), 0) + nvl(lengthb(l), 0) + 1 > 32767 then c := l; exit; end if; \
     b := b || l || chr(10); end loop; :1 := b; :2 := s; :3 := c; end;";

/// The notice for DBMS_OUTPUT lines (None when there are none).
pub(crate) fn output_notice(lines: &[String], more: bool) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let mut text = lines.join("\n");
    if more {
        text.push_str(&format!("\n… more DBMS_OUTPUT not shown (the first {} lines are)", lines.len()));
    }
    Some(text)
}

/// Adds a `:1` chunk of [`OUTPUT_SQL`] (and its carried line) to `lines`;
/// false when the caps are reached.
pub(crate) fn add_output(lines: &mut Vec<String>, bytes: &mut usize, chunk: Option<&str>, carry: Option<&str>) -> bool {
    let chunk = chunk.map(|c| c.strip_suffix('\n').unwrap_or(c));
    for l in chunk.into_iter().flat_map(|c| c.split('\n')).chain(carry) {
        if lines.len() >= OUTPUT_MAX_LINES || *bytes + l.len() > OUTPUT_MAX_BYTES {
            return false;
        }
        *bytes += l.len();
        lines.push(l.to_string());
    }
    true
}

/// DBMS_OUTPUT written by the statement just run (enabled at connect).
fn read_output(conn: &Connection) -> Option<String> {
    use oracle::sql_type::OracleType;
    let (mut lines, mut bytes) = (Vec::new(), 0);
    let mut more = false;
    loop {
        let Ok(mut st) = conn.statement(OUTPUT_SQL).build() else { break };
        if st.execute(&[&OracleType::Varchar2(32767), &OracleType::Int64, &OracleType::Varchar2(32767)]).is_err() {
            break;
        }
        let chunk: Option<String> = st.bind_value(1).ok().flatten();
        let done: i64 = st.bind_value(2).unwrap_or(1);
        let carry: Option<String> = st.bind_value(3).ok().flatten();
        if !add_output(&mut lines, &mut bytes, chunk.as_deref(), carry.as_deref()) {
            more = true;
            break;
        }
        if done != 0 {
            break;
        }
    }
    output_notice(&lines, more)
}

pub(crate) fn run(conn: &Connection, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
    // Oracle rejects a trailing semicolon in SQL (but PL/SQL blocks need theirs).
    let (sql, _) = prepare_sql(sql);
    let batch = opts.batch_size.clamp(1, 10_000);
    let mut stmt = conn.statement(sql).fetch_array_size(batch as u32).prefetch_rows(batch as u32 + 1).build().map_err(map_err)?;
    if !stmt.is_query() {
        let r = stmt.execute(&[]).map_err(map_err);
        // Printed before a failure too (what ran up to the error).
        if reads_output(sql)
            && let Some(n) = read_output(conn)
        {
            tx.blocking_send(Ok(StreamEvent::Notice(n)));
        }
        r?;
        let n = stmt.row_count().ok();
        tx.blocking_send(Ok(StreamEvent::Done(ExecSummary { rows_affected: n })));
        return Ok(());
    }
    let rows = stmt.query(&[]).map_err(map_err)?;
    let cols: Vec<Column> = rows.column_info().iter().map(|c| Column::new(c.name(), col_type(c.oracle_type()), db_type(c.oracle_type()))).collect();
    let types: Vec<ColType> = cols.iter().map(|c| c.col_type).collect();
    let mut b = BatchBuilder::new(&cols, batch);
    if !tx.blocking_send(Ok(StreamEvent::Schema(b.schema()))) {
        return Ok(());
    }
    for (n, row) in rows.enumerate() {
        if opts.max_rows.is_some_and(|m| n >= m) {
            break; // the caller keeps no more
        }
        let row = row.map_err(map_err)?;
        b.push_row(row.sql_values().iter().zip(&types).map(|(v, t)| convert(*t, v)));
        if b.is_full() && !tx.blocking_send(Ok(StreamEvent::Batch(b.finish()?))) {
            return Ok(());
        }
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
    }
    if !b.is_empty() && !tx.blocking_send(Ok(StreamEvent::Batch(b.finish()?))) {
        return Ok(());
    }
    for n in b.notices(None) {
        tx.blocking_send(Ok(StreamEvent::Notice(n)));
    }
    tx.blocking_send(Ok(StreamEvent::Done(ExecSummary::default())));
    Ok(())
}

/// Run a statement through the thin driver, streaming batches as they arrive.
pub(crate) fn thin_execute(a: Arc<agent::Agent>, sql: &str, opts: ExecOptions) -> QueryStream {
    let (tx, stream) = QueryStream::channel(4);
    let (sql, is_query) = prepare_sql(sql);
    let sql = sql.to_string();
    tokio::spawn(async move {
        if let Err(e) = thin_run(&a, &sql, is_query, &opts, &tx).await {
            // Stopped: said as stopped, keeping "session ended" when it was.
            let e = if opts.cancel.is_cancelled() && !e.is_cancelled() { ConnectorError::cancelled() } else { e };
            tx.send_err(e).await;
        }
    });
    stream
}

/// How long a stopped query may take to end on the thin driver before its
/// helper is ended (the helper itself gives up after 5 s). Not a DML or a
/// block: Oracle answers its stop after rolling it back.
const CANCEL_WAIT: std::time::Duration = std::time::Duration::from_millis(if cfg!(test) { 300 } else { 10_000 });

pub(crate) async fn thin_run(a: &agent::Agent, sql: &str, is_query: bool, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
    use agent::Frame;
    let batch = opts.batch_size.clamp(1, 10_000);
    let req = serde_json::json!({
        "op": if is_query { "query" } else { "exec" },
        "sql": sql,
        "batch_size": batch,
        "max_rows": opts.max_rows.unwrap_or(0),
        "output": !is_query && reads_output(sql),
    });
    /// The DBMS_OUTPUT a reply carries, as a notice.
    fn output_of(j: &serde_json::Value) -> Option<String> {
        let lines: Vec<String> = j["output"].as_array()?.iter().map(|l| l.as_str().unwrap_or("").to_string()).collect();
        output_notice(&lines, j["output_more"].as_bool().unwrap_or(false))
    }
    // Stopped while another statement holds the session: never sent.
    let (id, mut r) = a.start_unless(req, &opts.cancel).await?;
    let mut builder: Option<(BatchBuilder, usize)> = None;
    let mut cancelled = false;
    // Stop asked (Stop button / row cap / consumer gone): tell the helper,
    // then keep reading until it ends the request. Dropping a read in
    // progress for it is safe: the reader keeps a part-read frame. A stop
    // that isn't answered (the helper gives up after 5 s and exits) ends
    // the helper here: nothing else would free the session, and every
    // later statement on it would wait behind this one.
    let mut stopped_at: Option<tokio::time::Instant> = None;
    loop {
        // Queries only: a stopped DML or block answers once Oracle has rolled
        // it back, which takes as long as it ran; ending the session then
        // would lose the transaction's other work.
        let deadline = stopped_at.filter(|_| is_query).map(|t| t + CANCEL_WAIT);
        let frame = tokio::select! {
            f = agent::Agent::next(&mut r, id) => match f {
                // The helper exited (it gave up on a stop): stopped, not failed.
                Err(_) if cancelled => return Err(ConnectorError::cancelled_session_ended()),
                f => f?,
            },
            _ = opts.cancel.cancelled(), if !cancelled => {
                cancelled = true;
                stopped_at = Some(tokio::time::Instant::now());
                a.cancel(id).await;
                continue;
            }
            _ = async { tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)).await }, if deadline.is_some() => {
                a.kill();
                return Err(ConnectorError::cancelled_session_ended());
            }
        };
        match frame {
            Frame::Json(j) => match j["type"].as_str() {
                Some("schema") => {
                    let cols: Vec<Column> = j["columns"]
                        .as_array()
                        .map(|c| {
                            c.iter()
                                .map(|c| {
                                    let t = match c["type"].as_str() {
                                        Some("int64") => ColType::Int64,
                                        Some("float64") => ColType::Float64,
                                        Some("timestamp") => ColType::Timestamp,
                                        Some("timestamptz") => ColType::TimestampTz,
                                        Some("binary") => ColType::Binary,
                                        Some("bool") => ColType::Bool,
                                        _ => ColType::Utf8,
                                    };
                                    Column::new(c["name"].as_str().unwrap_or(""), t, c["db_type"].as_str().unwrap_or("").to_string())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let b = BatchBuilder::new(&cols, batch);
                    if !cancelled && !tx.send(StreamEvent::Schema(b.schema())).await {
                        cancelled = true;
                        stopped_at = Some(tokio::time::Instant::now());
                        a.cancel(id).await;
                    }
                    builder = Some((b, cols.len()));
                }
                Some("done") => {
                    if cancelled {
                        return Err(ConnectorError::cancelled());
                    }
                    if let Some((b, _)) = builder.as_mut()
                        && !b.is_empty()
                    {
                        tx.send(StreamEvent::Batch(b.finish()?)).await;
                    }
                    for n in builder.as_ref().map(|(b, _)| b.notices(None)).unwrap_or_default().into_iter().chain(output_of(&j)) {
                        tx.send(StreamEvent::Notice(n)).await;
                    }
                    let rows_affected = j["rows_affected"].as_u64();
                    tx.send(StreamEvent::Done(ExecSummary { rows_affected })).await;
                    return Ok(());
                }
                Some("error") => {
                    // What a failed block printed before the error.
                    if let Some(n) = output_of(&j) {
                        tx.send(StreamEvent::Notice(n)).await;
                    }
                    return Err(agent::reply_error(&j));
                }
                _ => {}
            },
            Frame::Batch { ncols, nrows, data, .. } => {
                if cancelled {
                    continue;
                }
                let Some((b, n)) = builder.as_mut() else { continue };
                if *n != ncols {
                    return Err(ConnectorError::internal("Oracle driver sent a batch with the wrong column count"));
                }
                agent::decode_rows(&data, ncols, nrows, |row| b.push_row(row))?;
                if b.is_full() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
                    cancelled = true;
                    stopped_at = Some(tokio::time::Instant::now());
                    a.cancel(id).await;
                }
            }
        }
    }
}

pub(crate) fn tokio_util_token() -> databrain_connector_core::CancellationToken {
    databrain_connector_core::CancellationToken::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PL/SQL keeps its final `;` wherever `end;` stands; SQL loses it, a
    /// `CASE … END` in a view included.
    #[test]
    fn plsql_units_keep_their_semicolon() {
        for (sql, keep) in [
            ("create or replace procedure p is\nbegin\n  null;\nend;", true),
            ("create procedure p as begin null;\nend;\n", true),
            ("CREATE OR REPLACE EDITIONABLE FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END;", true),
            ("create or replace package body pk as procedure x is begin null; end;\nend;", true),
            ("create trigger t before insert on x for each row\nbegin null;\nend;", true),
            ("-- note\ncreate /* c */ or replace procedure p is begin null; end;", true),
            ("begin\n  null;\nend;", true),
            ("declare x number; begin null; end;", true),
            ("create or replace view v as select case when a > 1 then 'x' end as c from t;", false),
            ("create table t (a number);", false),
            ("select 1 from dual;", false),
            ("create view \"PROCEDURE\" as select 1 x from dual;", false),
        ] {
            let (out, _) = prepare_sql(sql);
            assert_eq!(out.ends_with(';'), keep, "{sql:?} -> {out:?}");
        }
    }

    /// DBMS_OUTPUT chunks become lines (empty ones kept), a carried line
    /// follows its chunk, and the caps stop the reading with a note.
    #[test]
    fn output_lines_and_caps() {
        let (mut lines, mut bytes) = (Vec::new(), 0);
        assert!(add_output(&mut lines, &mut bytes, Some("a\n\nb\n"), Some("long")));
        assert!(add_output(&mut lines, &mut bytes, None, None));
        assert_eq!(lines, ["a", "", "b", "long"]);
        assert_eq!(output_notice(&lines, false).as_deref(), Some("a\n\nb\nlong"));
        assert_eq!(output_notice(&[], false), None);

        let (mut lines, mut bytes) = (Vec::new(), 0);
        let many = "x\n".repeat(OUTPUT_MAX_LINES + 5);
        assert!(!add_output(&mut lines, &mut bytes, Some(&many), None));
        assert_eq!(lines.len(), OUTPUT_MAX_LINES);
        assert!(output_notice(&lines, true).unwrap().ends_with("… more DBMS_OUTPUT not shown (the first 10000 lines are)"));
    }

    #[test]
    fn output_is_read_after_blocks_and_calls_only() {
        assert!(reads_output("begin dbms_output.put_line('x'); end;"));
        assert!(reads_output("-- c\ndeclare n number; begin null; end;"));
        assert!(reads_output("call p()"));
        assert!(!reads_output("select 1 from dual"));
        assert!(!reads_output("insert into t values (1)"));
    }
}

#[cfg(all(test, unix))]
mod stop_tests {
    use super::*;

    /// A helper that never answers a stop (a read waiting for bytes that
    /// never come) is ended after CANCEL_WAIT: the statement ends as
    /// stopped with the session ended (the engine opens a new one), and the
    /// process, with its connection to Oracle, is gone.
    #[tokio::test]
    async fn an_unanswered_stop_ends_the_helper() {
        let child = tokio::process::Command::new("sh")
            .args(["-c", "cat > /dev/null"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let a = agent::Agent::from_child(child);
        let cancel = databrain_connector_core::CancellationToken::new();
        let opts = ExecOptions { cancel: cancel.clone(), ..ExecOptions::default() };
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            c.cancel();
        });
        let started = std::time::Instant::now();
        // Through thin_execute, as a run does: the error it passes on keeps the code.
        let mut stream = thin_execute(a.clone(), "select doc from t", opts);
        let e = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match stream.next().await {
                    Some(Err(e)) => break e,
                    Some(Ok(_)) => continue,
                    None => panic!("ended without an error"),
                }
            }
        })
        .await
        .expect("the stop was waited on forever");
        assert!(e.is_cancelled() && e.code.as_deref() == Some(databrain_connector_core::SESSION_ENDED), "{e:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
        for _ in 0..50 {
            if a.exited() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the helper process is still running");
    }
}
