use std::io::Cursor;

use databrain_connector_core::arrow::ipc::reader::StreamReader;
use databrain_connector_core::external::ExternalTablesSlot;
use databrain_connector_core::{ConnectorError, ExecOptions, ExecSummary, Result, StreamEvent, StreamSender};

use crate::database::Loaded;
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

pub(crate) fn run(
    conn: &duckdb::Connection,
    sql: &str,
    opts: &ExecOptions,
    tx: &StreamSender,
    outputs: Option<&ExternalTablesSlot>,
    loaded: &Loaded,
    shared: bool,
) -> Result<()> {
    for n in load_outputs(conn, sql, outputs, loaded, opts.output_masks.as_deref(), shared)? {
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
    let kw = databrain_connector_core::sql::leading_keyword(sql);
    let returns_rows = matches!(
        kw.as_str(),
        "SELECT" | "WITH" | "FROM" | "VALUES" | "TABLE" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "SUMMARIZE" | "PRAGMA" | "PIVOT" | "UNPIVOT" | "CALL"
    ) || lower.contains(" returning ");
    if !returns_rows {
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
    while let Some(array) = stmt.step().map_err(map_err)? {
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
        let b = arrow58::record_batch::RecordBatch::from(&array);
        // Re-chunk to the requested batch size.
        let size = opts.batch_size.max(1);
        let mut off = 0;
        while off < b.num_rows() {
            let len = size.min(b.num_rows() - off);
            if !tx.blocking_send(Ok(StreamEvent::Batch(convert_batch(&b.slice(off, len))?))) {
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

    use crate::test_util::*;
    use databrain_auth::AuthMethod;

    use databrain_connector_core::{ConnectionConfig, ConnectorKind, ExecOptions};

    #[tokio::test]
    async fn batches_and_cancel() {
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let r = s.execute("select * from range(5000)", ExecOptions { batch_size: 1000, ..Default::default() }).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), 5000);
        assert!(r.batches.iter().all(|b| b.num_rows() <= 1000));
        let cancel = databrain_connector_core::CancellationToken::new();
        let st = s
            .execute("select count(*) from range(10000000000) a, range(1000) b", ExecOptions { batch_size: 10, cancel: cancel.clone(), ..Default::default() })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();
        let e = tokio::time::timeout(std::time::Duration::from_secs(10), st.collect()).await.unwrap().unwrap_err();
        assert!(e.is_cancelled(), "{e:?}");
        assert_eq!(q(s.as_ref(), "select 1").await.num_rows(), 1);
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
                "select a.range, md5(a.range::varchar) from range(2000000000) a",
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
        drop(st);
        let r = s
            .execute("select case when range < 300000 then range else error('boom at ' || range) end as v from range(400000)", ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(r.unwrap_err().to_string().contains("boom"));
        assert_eq!(q(s.as_ref(), "select 1").await.num_rows(), 1, "the session still works");
    }

    /// New sessions spill into the app's folder within the memory limit it
    /// sets, sandboxed ones too: a sort bigger than memory finishes there,
    /// where DuckDB's default (`.tmp` in the current directory, `/` for an app
    /// started from Finder) failed.
    #[tokio::test]
    async fn sessions_spill_into_the_apps_folder() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("duckdb_spill").to_string_lossy().into_owned();
        crate::set_spill_dir(Some(spill.clone()));
        crate::set_memory_limit(Some(200 * 1024 * 1024));
        let mut sandboxed = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        sandboxed.options.insert(databrain_connector_core::SANDBOX_OPTION.into(), "1".into());
        let plain = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let sandboxed = session(sandboxed).await;
        crate::set_spill_dir(None);
        crate::set_memory_limit(None);
        for s in [plain.as_ref(), sandboxed.as_ref()] {
            let r = q(s, "select current_setting('temp_directory'), current_setting('memory_limit')").await;
            let b = &r.batches[0];
            let col = |i: usize| databrain_connector_core::arrow::array::AsArray::as_string::<i32>(b.column(i)).value(0).to_string();
            assert_eq!(col(0), spill);
            assert!(col(1).starts_with("190") || col(1).starts_with("200"), "{}", col(1));
            // ~1 GB of rows to sort with 200 MB: spilled, not failed.
            let r = q(s, "select count(*) from (select range, md5(range::varchar) s from range(8000000) order by s)").await;
            assert_eq!(r.num_rows(), 1);
        }
    }
}
