use std::io::Cursor;

use databrain_connector_core::arrow::ipc::reader::StreamReader;
use databrain_connector_core::external::ExternalTablesSlot;
use databrain_connector_core::{ConnectorError, ExecOptions, ExecSummary, Result, StreamEvent, StreamSender};

use crate::engine::load_extension;
use crate::error::map_err;
use crate::outputs::load_outputs;

/// Convert an arrow-58 batch to the workspace's Arrow via IPC.
pub(crate) fn convert_batch(b: &arrow58::record_batch::RecordBatch) -> Result<databrain_connector_core::arrow::array::RecordBatch> {
    let mut buf = Vec::new();
    {
        let mut w = arrow58::ipc::writer::StreamWriter::try_new(&mut buf, &b.schema()).map_err(|e| ConnectorError::internal(e.to_string()))?;
        w.write(b).map_err(|e| ConnectorError::internal(e.to_string()))?;
        w.finish().map_err(|e| ConnectorError::internal(e.to_string()))?;
    }
    let mut r = StreamReader::try_new(Cursor::new(buf), None)?;
    r.next().ok_or_else(|| ConnectorError::internal("empty IPC stream"))?.map_err(Into::into)
}

/// Whether a statement that ran (and failed) may be run a second time to
/// find its error: only one the classifier calls a read.
/// Whether `sql` gives rows: a query, or a write with `RETURNING` (as a
/// word of the code, after any space or line break; not in a string or a
/// comment).
fn returns_rows(sql: &str) -> bool {
    let kw = databrain_connector_core::sql::leading_keyword(sql);
    matches!(
        kw.as_str(),
        "SELECT" | "WITH" | "FROM" | "VALUES" | "TABLE" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "SUMMARIZE" | "PRAGMA" | "PIVOT" | "UNPIVOT" | "CALL"
    ) || databrain_connector_core::sql::code_text(sql, databrain_connector_core::ConnectorKind::Duckdb)
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|w| w.eq_ignore_ascii_case("returning"))
}

fn safe_to_run_again(sql: &str) -> bool {
    databrain_connector_core::sql::classify(sql, databrain_connector_core::ConnectorKind::Duckdb).kind.is_read()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    conn: &duckdb::Connection,
    sql: &str,
    opts: &ExecOptions,
    tx: &StreamSender,
    outputs: Option<&ExternalTablesSlot>,
    shared: bool,
    sandboxed: bool,
) -> Result<()> {
    if sandboxed {
        crate::sandbox::refuse_file_writes(sql)?;
        if shared {
            crate::sandbox::refuse_shared_views(sql)?;
        }
    }
    for n in load_outputs(conn, sql, outputs, opts.output_masks.as_deref(), shared, sandboxed)? {
        tx.blocking_send(Ok(StreamEvent::Notice(n)));
    }
    // Load the extension a query needs (bundled copy first).
    let lower = sql.to_ascii_lowercase();
    for (f, ext) in [("delta_scan", "delta"), ("iceberg_", "iceberg"), ("read_xlsx", "excel")] {
        if lower.contains(f) {
            load_extension(conn, ext);
        }
    }
    let mut stmt = conn.prepare(sql).map_err(map_err)?;
    if !returns_rows(sql) {
        let n = stmt.execute([]).map_err(map_err)?;
        tx.blocking_send(Ok(StreamEvent::Done(ExecSummary { rows_affected: Some(n as u64) })));
        return Ok(());
    }
    // Streamed: chunks are fetched as they are read, so a run that stops at
    // the row limit (or is cancelled) doesn't wait for the whole result,
    // which `query_arrow` would build first (a whole file, for a read without
    // LIMIT). `step` reports a failed chunk as an error.
    let schema = stmt.stream_arrow([]).map_err(map_err)?.get_schema();
    // Schema first (even for empty results).
    let empty = arrow58::record_batch::RecordBatch::new_empty(schema);
    let first = convert_batch(&empty)?;
    if !tx.blocking_send(Ok(StreamEvent::Schema(first.schema()))) {
        return Ok(());
    }
    // A stopped or abandoned query is wound down while it's still the
    // active one: interrupted, then read until DuckDB has stopped its work.
    // Dropped mid-way, its tasks could finish later and interrupt the
    // session's next statement.
    let wind_down = |stmt: &duckdb::Statement<'_>| {
        conn.interrupt_handle().interrupt();
        while let Ok(Some(_)) = stmt.step() {}
    };
    loop {
        let array = match stmt.step() {
            Ok(Some(a)) => a,
            Ok(None) => break,
            Err(e) => {
                let err = map_err(e);
                if !err.is_cancelled() || opts.cancel.is_cancelled() {
                    return Err(err);
                }
                // Interrupted, but not by the user: DuckDB stopped the query
                // over an error found by another of its threads. A read is run
                // once more, whole, to report that error itself; anything
                // else may have written already (`WITH … INSERT`), and a
                // second run would write again.
                let plain = safe_to_run_again(sql);
                let real = plain.then(|| conn.prepare(sql).and_then(|mut s| s.query_arrow([]).map(|r| r.count()))).and_then(|r| r.err());
                return Err(match real.map(map_err) {
                    Some(e) if !e.is_cancelled() => e,
                    _ => ConnectorError::query("DuckDB stopped the query while reading its rows (interrupted); run it again"),
                });
            }
        };
        if opts.cancel.is_cancelled() {
            wind_down(&stmt);
            return Err(ConnectorError::cancelled());
        }
        let b = arrow58::record_batch::RecordBatch::from(&array);
        // Re-chunk to the requested batch size.
        let size = opts.batch_size.max(1);
        let mut off = 0;
        while off < b.num_rows() {
            let len = size.min(b.num_rows() - off);
            if !tx.blocking_send(Ok(StreamEvent::Batch(convert_batch(&b.slice(off, len))?))) {
                // Nobody reads the rows any more.
                wind_down(&stmt);
                return Ok(());
            }
            off += len;
        }
    }
    tx.blocking_send(Ok(StreamEvent::Done(ExecSummary::default())));
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::{returns_rows, safe_to_run_again};
    use crate::test_util::*;
    use databrain_auth::AuthMethod;

    use databrain_connector_core::{ConnectionConfig, ConnectorKind, ExecOptions};

    #[tokio::test]
    async fn batches_and_cancel() {
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let r = s
            .execute(databrain_connector_core::Statement::internal("select * from range(5000)"), ExecOptions { batch_size: 1000, ..Default::default() })
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), 5000);
        assert!(r.batches.iter().all(|b| b.num_rows() <= 1000));
        let cancel = databrain_connector_core::CancellationToken::new();
        let st = s
            .execute(
                databrain_connector_core::Statement::internal("select count(*) from range(10000000000) a, range(1000) b"),
                ExecOptions { batch_size: 10, cancel: cancel.clone(), ..Default::default() },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();
        let e = tokio::time::timeout(std::time::Duration::from_secs(10), st.collect()).await.unwrap().unwrap_err();
        assert!(e.is_cancelled(), "{e:?}");
        assert_eq!(q(s.as_ref(), "select 1").await.num_rows(), 1);
    }

    /// `RETURNING` after a line break, a tab or a parenthesis gives rows; the
    /// word in a string or a comment doesn't.
    #[test]
    fn returning_anywhere_in_the_code() {
        for sql in [
            "insert into t values (1)\nreturning *",
            "insert into t values (1)\treturning a",
            "update t set a = 1\nRETURNING(a)",
            "delete from t where a = 1 returning *",
            "select 1",
        ] {
            assert!(returns_rows(sql), "{sql:?}");
        }
        for sql in [
            "insert into t values ('returning')",
            "insert into t values (1) -- returning *",
            "update t set a = 1 /* returning a */",
            "insert into t (returning_x) values (1)",
            "create table t (a int)",
        ] {
            assert!(!returns_rows(sql), "{sql:?}");
        }
    }

    #[tokio::test]
    async fn insert_returning_after_a_newline_gives_its_rows() {
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        q(s.as_ref(), "create table t (a int)").await;
        let r = q(s.as_ref(), "insert into t values (7), (8)\nreturning a * 10 as x").await;
        assert_eq!(r.num_rows(), 2);
        assert_eq!(r.batches[0].schema().field(0).name(), "x");
    }

    /// After DuckDB interrupts itself, only a read is run again to find the
    /// error: a `WITH … INSERT` (or any write) may have committed by then.
    #[test]
    fn only_reads_run_again() {
        for sql in ["select 1", "with x as (select 1) select * from x", "from range(3)", "values (1)"] {
            assert!(safe_to_run_again(sql), "{sql}");
        }
        for sql in [
            "with x as (select 1 a) insert into t select a from x",
            "with x as (select 1) update t set a = 1",
            "with x as (select 1) delete from t",
            "insert into t select * from range(10)",
            "create table t as select 1",
            "copy (select 1) to 'x.csv'",
            "select * from t; insert into t values (1)",
        ] {
            assert!(!safe_to_run_again(sql), "{sql}");
        }
    }

    /// Rows come as they are made: the first batch of a huge result arrives
    /// before the rest is computed, and a failure part way through is an
    /// error (not a panic), after the rows before it.
    #[tokio::test]
    async fn results_stream() {
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let cancel = databrain_connector_core::CancellationToken::new();
        let mut st = s
            .execute(
                databrain_connector_core::Statement::internal("select a.range, md5(a.range::varchar) from range(2000000000) a"),
                ExecOptions { batch_size: 1000, cancel: cancel.clone(), ..Default::default() },
            )
            .await
            .unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(ev) = st.next().await {
                if let databrain_connector_core::StreamEvent::Batch(b) = ev.unwrap() {
                    return b.num_rows();
                }
            }
            0
        })
        .await
        .expect("the first rows come before the whole result is made");
        assert_eq!(first, 1000);
        cancel.cancel();
        // Read the stopped stream to its end before the next statement, as
        // the engine does: otherwise the interrupt can hit the next one.
        while st.next().await.is_some() {}
        let r = s
            .execute(
                databrain_connector_core::Statement::internal(
                    "select case when range < 300000 then range else error('boom at ' || range) end as v from range(400000)",
                ),
                ExecOptions::default(),
            )
            .await
            .unwrap()
            .collect()
            .await;
        let e = r.unwrap_err().to_string();
        assert!(e.contains("boom"), "{e}");
        assert_eq!(q(s.as_ref(), "select 1").await.num_rows(), 1, "the session still works");
    }

    /// New sessions spill into the app's folder within the memory limit it
    /// sets, sandboxed ones too: a sort bigger than memory finishes there,
    /// where DuckDB's default (`.tmp` in the current directory, `/` for an app
    /// started from Finder) failed.
    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_spill_into_the_apps_folder() {
        let _g = crate::engine::SPILL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("duckdb_spill").to_string_lossy().into_owned();
        crate::set_spill_dir(Some(spill.clone()));
        crate::set_memory_limit(Some(200 * 1024 * 1024));
        let mine = crate::engine::spill_dir().unwrap().to_string_lossy().into_owned();
        let mut sandboxed = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        sandboxed.options.insert(databrain_connector_core::SANDBOX_OPTION.into(), "1".into());
        let plain = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let other = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let sandboxed = session(sandboxed).await;
        crate::set_spill_dir(None);
        crate::set_memory_limit(None);
        let dir_of = |r: &databrain_connector_core::Collected| {
            databrain_connector_core::arrow::array::AsArray::as_string::<i32>(r.batches[0].column(0)).value(0).to_string()
        };
        let mut dirs = Vec::new();
        for s in [plain.as_ref(), other.as_ref(), sandboxed.as_ref()] {
            let r = q(s, "select current_setting('temp_directory'), current_setting('memory_limit')").await;
            let b = &r.batches[0];
            let col = |i: usize| databrain_connector_core::arrow::array::AsArray::as_string::<i32>(b.column(i)).value(0).to_string();
            // Each in a folder of its own under this process's (DuckDB's temp
            // file names repeat in every instance).
            assert!(col(0).starts_with(&format!("{mine}{}", std::path::MAIN_SEPARATOR)), "{} under {mine}", col(0));
            assert!(mine.starts_with(&spill));
            assert!(col(1).starts_with("190") || col(1).starts_with("200"), "{}", col(1));
            dirs.push(dir_of(&r));
        }
        dirs.sort();
        dirs.dedup();
        assert_eq!(dirs.len(), 3, "no two instances share a spill folder: {dirs:?}");
        assert!(dirs.iter().any(|d| d.contains("sandbox-")));
        // ~1 GB of rows to sort with 200 MB in each, at the same time:
        // spilled, not failed, and neither reads the other's blocks.
        let sort = "select sum(i * range)::varchar from (select range, row_number() over (order by md5(range::varchar)) i from range(8000000))";
        let (a, b, c) = tokio::join!(q(plain.as_ref(), sort), q(other.as_ref(), sort), q(sandboxed.as_ref(), sort));
        assert_eq!(dir_of(&a), dir_of(&b));
        assert_eq!(dir_of(&a), dir_of(&c));
    }
}
