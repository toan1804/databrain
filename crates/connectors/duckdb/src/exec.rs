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
    let arrow = stmt.query_arrow([]).map_err(map_err)?;
    let schema = arrow.get_schema();
    // Schema first (even for empty results).
    let empty = arrow58::record_batch::RecordBatch::new_empty(schema);
    let first = convert_batch(&empty)?;
    if !tx.blocking_send(Ok(StreamEvent::Schema(first.schema()))) {
        return Ok(());
    }
    for b in arrow {
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
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
}
