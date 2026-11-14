//! Streaming a statement's result: disposition choice, fallbacks, Arrow decoding.

use std::collections::HashMap;
use std::sync::Arc;

use databrain_connector_core::arrow::array::{Array, RecordBatch};
use databrain_connector_core::arrow::datatypes::SchemaRef;
use databrain_connector_core::{ConnectorError, ExecOptions, ExecSummary, Result, StreamEvent, StreamSender};

use crate::common::{RowSink, json_cell};

use super::client::Inner;
use super::download::*;
use super::types::manifest_columns;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Disposition {
    Inline,
    /// EXTERNAL_LINKS with JSON_ARRAY chunks (fallback when Arrow is refused).
    ExternalLinks,
    /// EXTERNAL_LINKS with ARROW_STREAM chunks, downloaded in parallel (default).
    ArrowLinks,
}

impl Disposition {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Disposition::Inline => "INLINE",
            Disposition::ExternalLinks | Disposition::ArrowLinks => "EXTERNAL_LINKS",
        }
    }
    pub(super) fn format(self) -> &'static str {
        match self {
            Disposition::ArrowLinks => "ARROW_STREAM",
            _ => "JSON_ARRAY",
        }
    }
}

/// Result chunks downloaded at the same time (like the Python connector's
/// CloudFetch). Overridable per connection with the `fetch_parallel` option.
pub(super) const FETCH_PARALLEL: usize = 8;

/// Why a streamed statement failed: external links could not be used
/// (retry inline), or anything else.
pub(super) enum StreamFail {
    Links(ConnectorError),
    /// The server refused ARROW_STREAM: retry with JSON chunks.
    Format(ConnectorError),
    Other(ConnectorError),
}

impl StreamFail {
    pub(super) fn into_inner(self) -> ConnectorError {
        match self {
            StreamFail::Links(e) | StreamFail::Format(e) | StreamFail::Other(e) => e,
        }
    }
}

/// Server errors about the requested result format.
pub(super) fn mentions_format(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("arrow_stream") || m.contains("arrow stream") || (m.contains("format") && m.contains("not supported"))
}

impl Inner {
    pub(super) async fn stream(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
        // INLINE results are capped at 25 MiB by the server ("Inline byte limit
        // exceeded"), so results go through EXTERNAL_LINKS (cloud-storage URLs,
        // up to 100 GiB) as Arrow, downloaded in parallel. If Arrow is refused,
        // JSON chunks are used; workspaces that block the links fall back to INLINE.
        if self.inline_only {
            return self.stream_as(sql, opts, tx, Disposition::Inline).await.map_err(StreamFail::into_inner);
        }
        let mut r = self.stream_arrow(sql, opts, tx).await;
        if let Err(StreamFail::Format(_)) = r {
            r = self.stream_as(sql, opts, tx, Disposition::ExternalLinks).await;
        }
        match r {
            Err(StreamFail::Links(e)) => {
                tx.send(StreamEvent::Notice(format!(
                    "Could not download the result from cloud storage ({}); ran again with inline results (25 MiB limit). Set \"Result transfer\" to Inline on this connection to skip the first try.",
                    e.message
                )))
                .await;
                self.stream_as(sql, opts, tx, Disposition::Inline).await.map_err(StreamFail::into_inner)
            }
            r => r.map_err(StreamFail::into_inner),
        }
    }

    /// Submit and wait, classifying failures for the fallbacks.
    pub(super) async fn run_statement(&self, sql: &str, opts: &ExecOptions, disposition: Disposition) -> std::result::Result<serde_json::Value, StreamFail> {
        let classify = |e: ConnectorError| {
            if disposition == Disposition::ArrowLinks && mentions_format(&e.message) {
                StreamFail::Format(e)
            } else if disposition != Disposition::Inline && mentions_links(&e.message) {
                StreamFail::Links(e)
            } else {
                StreamFail::Other(e)
            }
        };
        let resp = self.submit_as(sql, opts.max_rows, disposition).await.map_err(classify)?;
        let resp = self.wait(resp, &opts.cancel).await.map_err(classify)?;
        if databrain_connector_core::sql::leading_keyword(sql) == "USE" {
            self.apply_use(sql).await;
        }
        Ok(resp)
    }

    /// EXTERNAL_LINKS + ARROW_STREAM: chunk links are resolved and downloaded
    /// `fetch_parallel` at a time, decoded off the async runtime, and sent in order.
    pub(super) async fn stream_arrow(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> std::result::Result<(), StreamFail> {
        use futures::StreamExt as _;
        let resp = self.run_statement(sql, opts, Disposition::ArrowLinks).await?;
        tx.send(StreamEvent::Downloading(links_notice(&resp))).await;
        let statement_id = resp["statement_id"].as_str().unwrap_or_default().to_string();
        let columns = manifest_columns(&resp);
        if columns.is_empty() {
            tx.send(StreamEvent::Done(ExecSummary::default())).await;
            return Ok(());
        }
        let schema = databrain_connector_core::value::schema_for(&columns);
        // Links the first response already carries (often only chunk 0).
        let mut known: HashMap<u64, ChunkLink> = HashMap::new();
        for l in resp.pointer("/result/external_links").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(link) = parse_link(l) {
                known.insert(link.chunk_index, link);
            }
        }
        let total = resp.pointer("/manifest/total_chunk_count").and_then(|v| v.as_u64()).unwrap_or_else(|| known.keys().max().map(|m| m + 1).unwrap_or(0));
        if total == 0 {
            // No rows: schema only.
            if tx.send(StreamEvent::Schema(schema)).await {
                tx.send(StreamEvent::Done(ExecSummary::default())).await;
            }
            return Ok(());
        }
        let known = Arc::new(known);
        let fetch = |i: u64| {
            let (known, id, schema) = (known.clone(), statement_id.clone(), schema.clone());
            async move {
                let link = known.get(&i).cloned();
                let bytes = self.chunk_bytes(&id, i, link).await?;
                tokio::task::spawn_blocking(move || decode_arrow(&bytes, &schema)).await.map_err(|e| ConnectorError::internal(e.to_string()))?
            }
        };
        // First chunk before the schema: a blocked link can still fall back.
        let first = fetch(0).await.map_err(StreamFail::Links)?;
        if !tx.send(StreamEvent::Schema(schema.clone())).await {
            return Ok(());
        }
        let send_all = |batches: Vec<RecordBatch>| async move {
            for b in batches {
                if b.num_rows() > 0 && !tx.send(StreamEvent::Batch(b)).await {
                    return false;
                }
            }
            true
        };
        if !send_all(first).await {
            return Ok(());
        }
        let parallel = self.fetch_parallel.max(1);
        let mut rest = futures::stream::iter(1..total).map(fetch).buffered(parallel);
        while let Some(r) = rest.next().await {
            if opts.cancel.is_cancelled() {
                return Err(StreamFail::Other(ConnectorError::cancelled()));
            }
            if !send_all(r.map_err(StreamFail::Other)?).await {
                return Ok(());
            }
        }
        tx.send(StreamEvent::Done(ExecSummary::default())).await;
        Ok(())
    }

    pub(super) async fn stream_as(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender, disposition: Disposition) -> std::result::Result<(), StreamFail> {
        let resp = self.run_statement(sql, opts, disposition).await?;
        tx.send(StreamEvent::Downloading(if disposition == Disposition::Inline { None } else { links_notice(&resp) })).await;
        let statement_id = resp["statement_id"].as_str().unwrap_or_default().to_string();
        let columns = manifest_columns(&resp);
        if columns.is_empty() {
            tx.send(StreamEvent::Done(ExecSummary::default())).await;
            return Ok(());
        }
        let mut chunk = resp.get("result").cloned();
        // Download the first external chunk before announcing the schema, so a
        // blocked link can still fall back to INLINE cleanly.
        let mut pending: Option<Vec<serde_json::Value>> = None;
        if let Some(c) = &chunk
            && let Some(link) = first_link(c)
        {
            match self.download(&statement_id, &link).await {
                Ok(rows) => pending = Some(rows),
                Err(e) => return Err(StreamFail::Links(e)),
            }
        }
        let Some(mut sink) = RowSink::start(&columns, opts.batch_size, tx).await else { return Ok(()) };
        'outer: while let Some(c) = chunk {
            let rows: Vec<serde_json::Value> = match pending.take() {
                Some(r) => r,
                None => self.chunk_rows(&statement_id, &c).await.map_err(StreamFail::Other)?,
            };
            for r in &rows {
                let row = r.as_array().cloned().unwrap_or_default();
                let vals = sink.types.clone().into_iter().enumerate().map(|(i, t)| json_cell(t, row.get(i).unwrap_or(&serde_json::Value::Null)));
                if !sink.push(vals).await.map_err(StreamFail::Other)? {
                    break 'outer;
                }
            }
            if opts.cancel.is_cancelled() {
                return Err(StreamFail::Other(ConnectorError::cancelled()));
            }
            chunk = self.next_chunk(&c).await.map_err(StreamFail::Other)?;
        }
        sink.finish().await.map_err(StreamFail::Other)?;
        tx.send(StreamEvent::Done(ExecSummary::default())).await;
        Ok(())
    }
}

/// Server errors that mean EXTERNAL_LINKS is not available for this workspace.
pub(super) fn mentions_links(msg: &str) -> bool {
    if is_inline_limit(msg) {
        return false;
    }
    let m = msg.to_ascii_lowercase();
    m.contains("external_links") || m.contains("external links") || m.contains("disposition")
}

/// "Inline byte limit exceeded" (INLINE results are capped at 25 MiB).
pub(super) fn is_inline_limit(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("inline byte limit") || (m.contains("inline") && m.contains("limit") && m.contains("external"))
}

/// "The data is large…" for a result handed out as external links.
pub(super) fn links_notice(resp: &serde_json::Value) -> Option<String> {
    let m = |p: &str| resp.pointer(p).and_then(|v| v.as_u64());
    let link = resp.pointer("/result/external_links/0/external_link").and_then(|v| v.as_str());
    crate::common::large_result_notice(
        "Databricks",
        m("/manifest/total_byte_count"),
        m("/manifest/total_row_count"),
        m("/manifest/total_chunk_count").unwrap_or(1),
        link,
    )
}

/// Decode an Arrow IPC stream chunk and conform its columns to `schema` (the
/// same types the JSON path produces: exact decimals and complex values as
/// text, timestamps in microseconds), so results look the same either way.
pub(super) fn decode_arrow(bytes: &[u8], schema: &SchemaRef) -> Result<Vec<RecordBatch>> {
    use databrain_connector_core::arrow::array::{ArrayRef, StringBuilder};
    use databrain_connector_core::arrow::compute::{CastOptions, cast_with_options};
    use databrain_connector_core::arrow::ipc::reader::StreamReader;
    use databrain_connector_core::arrow::util::display::{ArrayFormatter, FormatOptions};
    let err = |e: databrain_connector_core::arrow::error::ArrowError| ConnectorError::internal(format!("unexpected Arrow result chunk: {e}"));
    // A garbled chunk can claim a huge first message: check before allocating.
    let first_len = match bytes.get(..4) {
        Some([0xff, 0xff, 0xff, 0xff]) => bytes.get(4..8).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        Some(b) => Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        None => None,
    };
    if !first_len.is_some_and(|n| n >= 0 && (n as usize) <= bytes.len()) {
        return Err(ConnectorError::internal("unexpected Arrow result chunk: not an Arrow IPC stream"));
    }
    let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).map_err(err)?;
    let mut out = Vec::new();
    for b in reader {
        let b = b.map_err(err)?;
        if b.num_columns() != schema.fields().len() {
            return Err(ConnectorError::internal(format!("result chunk has {} columns, expected {}", b.num_columns(), schema.fields().len())));
        }
        let cols: Vec<ArrayRef> = b
            .columns()
            .iter()
            .zip(schema.fields())
            .map(|(col, f)| -> Result<ArrayRef> {
                if col.data_type() == f.data_type() {
                    return Ok(col.clone());
                }
                let direct = if col.data_type().is_nested() {
                    None
                } else {
                    cast_with_options(col, f.data_type(), &CastOptions { safe: true, ..Default::default() }).ok()
                };
                match direct {
                    Some(a) if a.null_count() == col.null_count() => Ok(a),
                    // Nested values or casts that lose data: their text form.
                    _ => {
                        let fmt = ArrayFormatter::try_new(col.as_ref(), &FormatOptions::default()).map_err(err)?;
                        let mut sb = StringBuilder::with_capacity(col.len(), col.len() * 8);
                        for i in 0..col.len() {
                            if col.is_null(i) {
                                sb.append_null();
                            } else {
                                sb.append_value(fmt.value(i).to_string());
                            }
                        }
                        let text: ArrayRef = Arc::new(sb.finish());
                        if f.data_type() == &databrain_connector_core::arrow::datatypes::DataType::Utf8 {
                            Ok(text)
                        } else {
                            cast_with_options(&text, f.data_type(), &CastOptions::default()).map_err(err)
                        }
                    }
                }
            })
            .collect::<Result<_>>()?;
        out.push(RecordBatch::try_new(schema.clone(), cols).map_err(err)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use databrain_connector_core::value::Column;
    use databrain_connector_core::{ColType, ExecOptions, Session};
    use serde_json::json;

    use super::super::testutil::{ints, mock, texts};
    use super::*;

    #[test]
    fn classifies_link_errors() {
        let inline = "Inline byte limit exceeded. Statements executed with disposition=INLINE can have a result size of at most 26214400 bytes. Please execute the Statement with disposition EXTERNAL_LINKS";
        assert!(is_inline_limit(inline));
        assert!(!mentions_links(inline), "the inline-limit error must not trigger the inline fallback");
        assert!(mentions_links("EXTERNAL_LINKS disposition is disabled for this workspace"));
        assert_eq!(redact_url("error sending request for url (https://s3.x/a/b?X-Amz-Signature=secret)"), "error sending request for url (https://s3.x/a/b?…");
        let c = json!({"external_links": [{"external_link": "https://x/y", "chunk_index": 3}]});
        let l = first_link(&c).unwrap();
        assert_eq!((l.url.as_str(), l.chunk_index), ("https://x/y", 3));
        assert!(first_link(&json!({"data_array": []})).is_none());
    }

    #[tokio::test]
    async fn arrow_chunks_download_in_parallel_and_in_order() {
        let (origin, log) = mock::serve("ok").await;
        let s = mock::session(&origin, false);
        let opts = ExecOptions { max_rows: Some(1001), ..Default::default() };
        let t0 = std::time::Instant::now();
        let r = s.execute(databrain_connector_core::Statement::internal("SELECT * FROM big"), opts).await.unwrap().collect().await.unwrap();
        let took = t0.elapsed();
        let n = mock::CHUNKS as i64;
        assert_eq!(ints(&r), (0..n * 2).collect::<Vec<_>>(), "rows in chunk order, n cast to BIGINT");
        assert_eq!(texts(&r)[..4], ["r0", "NULL", "r2", "NULL"]);
        assert_eq!(r.schema.as_ref().unwrap().field(0).data_type(), &databrain_connector_core::arrow::datatypes::DataType::Int64);
        assert_eq!(r.notices.len(), 1, "{:?}", r.notices);
        assert!(r.notices[0].starts_with(&format!("The data is large ({n} chunks), so it is downloaded from Databricks's external links")), "{}", r.notices[0]);
        let log = log.lock().unwrap();
        assert!(log.max_in_flight >= 3, "downloads overlap: {}", log.max_in_flight);
        // 6 chunks × 60 ms sequentially would be ≥ 360 ms.
        assert!(took < std::time::Duration::from_millis(330), "took {took:?}");
        let post: serde_json::Value = serde_json::from_str(&log.requests[0].3).unwrap();
        assert_eq!((post["disposition"].as_str(), post["format"].as_str()), (Some("EXTERNAL_LINKS"), Some("ARROW_STREAM")));
        assert_eq!(post["row_limit"], 1001, "row cap passed to the server");
        for (m, p, auth, _) in &log.requests {
            if p.starts_with("/blob/") {
                assert!(!auth, "presigned download must not carry the token: {m} {p}");
            } else {
                assert!(auth, "API call without token: {m} {p}");
            }
        }
    }

    #[tokio::test]
    async fn arrow_refused_uses_json_links() {
        let (origin, log) = mock::serve("no_arrow").await;
        let r = mock::session(&origin, false)
            .execute(databrain_connector_core::Statement::internal("SELECT * FROM big"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(texts(&r), vec!["r0", "NULL", "r2", "NULL"]);
        assert!(r.notices.is_empty(), "{:?}", r.notices);
        assert!(log.lock().unwrap().requests.iter().any(|(_, p, _, _)| p.starts_with("/blob/json/")));
    }

    #[test]
    fn decodes_and_conforms_arrow_chunks() {
        let cols = vec![Column::new("n", ColType::Int64, "bigint"), Column::new("s", ColType::Utf8, "string")];
        let schema = databrain_connector_core::value::schema_for(&cols);
        let b = decode_arrow(&mock::arrow_chunk(3), &schema).unwrap();
        assert_eq!(b[0].schema(), schema, "same schema (and type metadata) as the JSON path");
        assert_eq!(b[0].num_rows(), 2);
        assert!(decode_arrow(b"not arrow", &schema).is_err());
    }

    #[tokio::test]
    async fn expired_link_is_renewed() {
        let (origin, log) = mock::serve("expire_once").await;
        let s = mock::session(&origin, false);
        let r = s.execute(databrain_connector_core::Statement::internal("SELECT * FROM big"), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), mock::CHUNKS as usize * 2);
        assert!(log.lock().unwrap().requests.iter().any(|(_, p, _, _)| p == "/api/2.0/sql/statements/st1/result/chunks/0"), "fresh link requested");
    }

    #[tokio::test]
    async fn blocked_storage_falls_back_to_inline() {
        let (origin, _) = mock::serve("blocked").await;
        let s = mock::session(&origin, false);
        let r = s.execute(databrain_connector_core::Statement::internal("SELECT * FROM big"), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(texts(&r), vec!["inline"]);
        assert!(r.notices.iter().any(|n| n.contains("inline")), "{:?}", r.notices);
        // Inline-only connections never try links.
        let (origin, log) = mock::serve("ok").await;
        let r = mock::session(&origin, true)
            .execute(databrain_connector_core::Statement::internal("SELECT 1"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(texts(&r), vec!["inline"]);
        assert!(log.lock().unwrap().requests.iter().all(|(_, p, _, _)| !p.starts_with("/blob/")));
    }

    #[tokio::test]
    async fn dropped_connections_and_503_are_retried() {
        let (origin, log) = mock::serve("flaky").await;
        let r = mock::session(&origin, false)
            .execute(databrain_connector_core::Statement::internal("SELECT * FROM big"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(ints(&r), (0..mock::CHUNKS as i64 * 2).collect::<Vec<_>>());
        let log = log.lock().unwrap();
        assert!(log.hits.values().all(|h| *h == 3), "drop, 503, then ok for every chunk: {:?}", log.hits);
    }

    #[tokio::test]
    async fn expired_links_are_renewed_before_download() {
        let (origin, log) = mock::serve("stale_links").await;
        let r = mock::session(&origin, false)
            .execute(databrain_connector_core::Statement::internal("SELECT * FROM big"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), mock::CHUNKS as usize * 2);
        {
            let log = log.lock().unwrap();
            assert!(log.requests.iter().all(|(_, p, _, _)| !p.contains("sig=old")), "expired link never used");
            assert!(log.requests.iter().any(|(_, p, _, _)| p == "/api/2.0/sql/statements/st1/result/chunks/0"));
        }
        // JSON chunks too.
        let (origin, _) = mock::serve("stale_links").await;
        let s = mock::session(&origin, false);
        let r = s.0.stream_json_for_test("SELECT * FROM big").await;
        assert_eq!(r, vec!["r0", "NULL", "r2", "NULL"]);
    }

    impl Inner {
        /// JSON-chunk path (`Disposition::ExternalLinks`), column `s` as text.
        async fn stream_json_for_test(&self, sql: &str) -> Vec<String> {
            use databrain_connector_core::arrow::util::display::{ArrayFormatter, FormatOptions};
            let (tx, stream) = databrain_connector_core::QueryStream::channel(4);
            let opts = ExecOptions::default();
            let r = self.stream_as(sql, &opts, &tx, Disposition::ExternalLinks).await;
            assert!(r.is_ok());
            drop(tx);
            let c = stream.collect().await.unwrap();
            let mut out = vec![];
            for b in &c.batches {
                let f = ArrayFormatter::try_new(b.column(1).as_ref(), &FormatOptions::default().with_null("NULL")).unwrap();
                for i in 0..b.num_rows() {
                    out.push(f.value(i).to_string());
                }
            }
            out
        }
    }
}
