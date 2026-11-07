//! Fake Databricks + cloud storage on localhost, shared by the stream/download tests.

use std::sync::Arc;

use databrain_auth::AuthMethod;
use databrain_connector_core::arrow::array::RecordBatch;
use serde_json::json;
use tokio::sync::Mutex;

use crate::common::http;

use super::client::{DbxSession, Inner};

/// Fake Databricks + cloud storage on localhost.
pub mod mock {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub const CHUNKS: u32 = 6;

    #[derive(Default)]
    pub struct Log {
        pub requests: Vec<(String, String, bool, String)>, // method, path, had auth header, body
        pub arrow_posts: Option<u32>,
        /// Requests per blob path (without query).
        pub hits: std::collections::HashMap<String, u32>,
        pub in_flight: usize,
        pub max_in_flight: usize,
    }

    impl Log {
        fn enter(&mut self) -> usize {
            self.in_flight += 1;
            self.max_in_flight = self.max_in_flight.max(self.in_flight);
            self.in_flight
        }
        fn leave(&mut self) {
            self.in_flight -= 1;
        }
    }

    /// Arrow IPC chunk `i`: rows (2i, "r2i"), (2i+1, NULL), with `n` as
    /// Int32 so the client has to conform it to BIGINT.
    pub fn arrow_chunk(i: u32) -> Vec<u8> {
        use databrain_connector_core::arrow::array::{Int32Array, StringArray};
        use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int32, true), Field::new("s", DataType::Utf8, true)]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int32Array::from(vec![(i * 2) as i32, (i * 2 + 1) as i32])), Arc::new(StringArray::from(vec![Some(format!("r{}", i * 2)), None]))],
        )
        .unwrap();
        let mut buf = Vec::new();
        let mut w = databrain_connector_core::arrow::ipc::writer::StreamWriter::try_new(&mut buf, &schema).unwrap();
        w.write(&b).unwrap();
        w.finish().unwrap();
        drop(w);
        buf
    }

    /// `blob_mode`: "ok", "expire_once" (first blob GET → 403), "blocked" (every blob GET → 403).
    pub async fn serve(blob_mode: &'static str) -> (String, Arc<StdMutex<Log>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", l.local_addr().unwrap());
        let log = Arc::new(StdMutex::new(Log::default()));
        let (o, lg) = (origin.clone(), log.clone());
        let expired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else { break };
                let (o, lg, expired) = (o.clone(), lg.clone(), expired.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    // Read headers, then the body by Content-Length.
                    let (head, body) = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..i]).to_string();
                            let len = head
                                .lines()
                                .find_map(|h| h.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                                .unwrap_or(0);
                            while buf.len() < i + 4 + len {
                                let n = sock.read(&mut tmp).await.unwrap_or(0);
                                if n == 0 {
                                    break;
                                }
                                buf.extend_from_slice(&tmp[..n]);
                            }
                            break (head, String::from_utf8_lossy(&buf[i + 4..]).to_string());
                        }
                    };
                    let mut first = head.lines().next().unwrap_or("").split_whitespace();
                    let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
                    let auth = head.to_ascii_lowercase().contains("\r\nauthorization:");
                    lg.lock().unwrap().requests.push((method.clone(), path.clone(), auth, body.clone()));
                    let cols = json!({"columns": [{"name": "n", "type_name": "LONG", "type_text": "BIGINT"}, {"name": "s", "type_name": "STRING", "type_text": "STRING"}]});
                    // Arrow results: CHUNKS chunks (links via the chunk API); JSON: 2 chunks.
                    // "stale_links": the statement response carries already-expired links.
                    let link_at = |i: u32, next: bool, fmt: &str, fresh: bool| {
                        let (sig, exp) = if fresh {
                            ("secret", chrono::Utc::now() + chrono::Duration::minutes(10))
                        } else {
                            ("old", chrono::Utc::now() - chrono::Duration::minutes(1))
                        };
                        let mut e =
                            json!({"chunk_index": i, "external_link": format!("{o}/blob/{fmt}/{i}?sig={sig}"), "row_count": 2, "expiration": exp.to_rfc3339()});
                        if next {
                            e["next_chunk_internal_link"] = json!(format!("/api/2.0/sql/statements/st1/result/chunks/{}", i + 1));
                        }
                        json!({"external_links": [e]})
                    };
                    let stale = blob_mode == "stale_links";
                    let link = |i: u32, next: bool, fmt: &str| link_at(i, next, fmt, true);
                    let first = |fmt: &str, next: bool| link_at(0, next, fmt, !stale);
                    let mut raw: Option<Vec<u8>> = None;
                    let (status, out) = if method == "POST" && path == "/api/2.0/sql/statements" {
                        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                        if v["disposition"] == "INLINE" {
                            (
                                200,
                                json!({"statement_id": "st2", "status": {"state": "SUCCEEDED"}, "manifest": {"schema": cols}, "result": {"data_array": [["7", "inline"]]}}),
                            )
                        } else if v["format"] == "ARROW_STREAM" && blob_mode == "no_arrow" {
                            (400, json!({"message": "ARROW_STREAM format is not supported for this warehouse"}))
                        } else if v["format"] == "ARROW_STREAM" {
                            *lg.lock().unwrap().arrow_posts.get_or_insert(0) += 1;
                            (
                                200,
                                json!({"statement_id": "st1", "status": {"state": "SUCCEEDED"}, "manifest": {"schema": cols, "total_chunk_count": CHUNKS}, "result": first("arrow", false)}),
                            )
                        } else {
                            (200, json!({"statement_id": "st1", "status": {"state": "SUCCEEDED"}, "manifest": {"schema": cols}, "result": first("json", true)}))
                        }
                    } else if let Some(i) = path.strip_prefix("/api/2.0/sql/statements/st1/result/chunks/") {
                        let i: u32 = i.parse().unwrap();
                        let arrow = lg.lock().unwrap().arrow_posts.is_some();
                        if arrow { (200, link(i, false, "arrow")) } else { (200, link(i, i == 0, "json")) }
                    } else if let Some(rest) = path.strip_prefix("/blob/") {
                        let (fmt, rest) = rest.split_once('/').unwrap();
                        let i: u32 = rest.split('?').next().unwrap().parse().unwrap();
                        let hit = {
                            let mut g = lg.lock().unwrap();
                            let h = g.hits.entry(format!("{fmt}/{i}")).or_insert(0);
                            *h += 1;
                            *h
                        };
                        let deny = blob_mode == "blocked"
                            || blob_mode == "always_403"
                            || path.contains("sig=old")
                            || (blob_mode == "expire_once" && !expired.swap(true, std::sync::atomic::Ordering::SeqCst));
                        if blob_mode == "flaky" && hit == 1 {
                            // Connection dropped mid-request ("error sending request").
                            return;
                        }
                        if (blob_mode == "flaky" && hit == 2) || blob_mode == "always_503" {
                            let _ = sock.write_all(b"HTTP/1.1 503 Slow Down\r\nretry-after: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                            return;
                        }
                        if deny {
                            (403, json!({"error": "expired"}))
                        } else if fmt == "arrow" {
                            // Slow storage: parallel downloads overlap.
                            let now = lg.lock().unwrap().enter();
                            let _ = now;
                            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                            lg.lock().unwrap().leave();
                            raw = Some(arrow_chunk(i));
                            (200, json!(null))
                        } else {
                            (200, json!([[format!("{}", i * 2), format!("r{}", i * 2)], [format!("{}", i * 2 + 1), null]]))
                        }
                    } else {
                        (404, json!({"message": "not found"}))
                    };
                    let body = raw.unwrap_or_else(|| out.to_string().into_bytes());
                    let head =
                        format!("HTTP/1.1 {status} X\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        (origin, log)
    }

    pub fn session(origin: &str, inline_only: bool) -> DbxSession {
        use databrain_auth::InlineCredentialSource;
        DbxSession(Arc::new(Inner {
            base: format!("{origin}/api/2.0/sql/statements"),
            origin: origin.to_string(),
            warehouse: "w1".into(),
            creds: Arc::new(InlineCredentialSource::new(AuthMethod::ApiToken { user: None }, Some("tok".to_string().into()))),
            http: http(),
            catalog: Mutex::new(None),
            schema: Mutex::new(None),
            inline_only,
            fetch_parallel: 4,
            storage: crate::common::storage_http(),
        }))
    }
}

pub fn texts(r: &databrain_connector_core::Collected) -> Vec<String> {
    use databrain_connector_core::arrow::util::display::{ArrayFormatter, FormatOptions};
    let mut out = vec![];
    for b in &r.batches {
        let f = ArrayFormatter::try_new(b.column(1).as_ref(), &FormatOptions::default().with_null("NULL")).unwrap();
        for i in 0..b.num_rows() {
            out.push(f.value(i).to_string());
        }
    }
    out
}

pub fn ints(r: &databrain_connector_core::Collected) -> Vec<i64> {
    use databrain_connector_core::arrow::array::AsArray;
    use databrain_connector_core::arrow::datatypes::Int64Type;
    r.batches.iter().flat_map(|b| b.column(0).as_primitive::<Int64Type>().values().to_vec()).collect()
}
