#![allow(dead_code)]

use std::time::Duration;

use databrain_connector_core::value::{Column, parse_date, parse_datetime, parse_time};
use databrain_connector_core::{BatchBuilder, ColType, ConnectorError, ErrorKind, Result, StreamEvent, StreamSender, Value};

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(20))
        .pool_idle_timeout(Duration::from_secs(60))
        .user_agent(concat!("DataBrain/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

/// Client for presigned cloud-storage downloads (S3 / ADLS / GCS result chunks).
/// Idle connections are dropped after 10 s: object stores close idle
/// keep-alive connections early, and reusing a closed one fails with
/// "error sending request". Each attempt is bounded by its own timeout.
pub fn storage_http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(32)
        .tcp_keepalive(Duration::from_secs(20))
        .user_agent(concat!("DataBrain/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("storage http client")
}

/// Error text with its source chain ("error sending request: …: connection reset").
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        let t = s.to_string();
        if !msg.contains(&t) {
            msg.push_str(": ");
            msg.push_str(&t);
        }
        src = s.source();
    }
    msg
}

pub fn net_err(e: reqwest::Error) -> ConnectorError {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    ConnectorError::connection(msg)
}

/// Read a JSON response body, turning HTTP errors into connector errors.
pub async fn json_body(resp: reqwest::Response, service: &str) -> Result<serde_json::Value> {
    let status = resp.status();
    let text = resp.text().await.map_err(net_err)?;
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    if status.is_success() {
        if v.is_null() && !text.trim().is_empty() {
            return Err(ConnectorError::internal(format!("{service}: unexpected response: {}", truncate(&text, 300))));
        }
        return Ok(v);
    }
    let message = v
        .pointer("/error/message")
        .or(v.get("message"))
        .or(v.pointer("/status/error/message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| truncate(&text, 300));
    let kind = if status.as_u16() == 401 || status.as_u16() == 403 { ErrorKind::Auth } else if status.is_server_error() { ErrorKind::Connection } else { ErrorKind::Query };
    Err(ConnectorError::new(kind, format!("{service} ({status}): {message}")))
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}

/// Convert a JSON cell (string/number/bool/null/nested) to a Value for `t`.
pub fn json_cell(t: ColType, v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => match t {
            ColType::Int64 => n.as_i64().map(Value::Int).unwrap_or_else(|| Value::Text(n.to_string())),
            ColType::Float64 => n.as_f64().map(Value::Float).unwrap_or(Value::Null),
            _ => Value::Text(n.to_string()),
        },
        J::String(s) => text_cell(t, s),
        other => Value::Text(other.to_string()),
    }
}

/// Parse a textual cell into the column type (cloud APIs send strings).
pub fn text_cell(t: ColType, s: &str) -> Value {
    let fallback = || Value::Text(s.to_string());
    match t {
        ColType::Bool => match s {
            "true" | "TRUE" | "1" => Value::Bool(true),
            "false" | "FALSE" | "0" => Value::Bool(false),
            _ => fallback(),
        },
        ColType::Int64 => s.parse().map(Value::Int).unwrap_or_else(|_| fallback()),
        ColType::Float64 => s.parse().map(Value::Float).unwrap_or_else(|_| fallback()),
        ColType::Date => parse_date(s).map(Value::Date).unwrap_or_else(fallback),
        ColType::Timestamp => parse_datetime(s).map(Value::Timestamp).unwrap_or_else(fallback),
        ColType::TimestampTz => parse_datetime(s).map(Value::TimestampTz).unwrap_or_else(fallback),
        ColType::Time => parse_time(s).map(Value::Time).unwrap_or_else(fallback),
        ColType::Binary => decode_hex(s).map(Value::Bytes).unwrap_or_else(fallback),
        _ => fallback(),
    }
}

pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Streams rows into Arrow batches.
pub struct RowSink<'a> {
    pub builder: BatchBuilder,
    pub types: Vec<ColType>,
    pub tx: &'a StreamSender,
    pub rows: usize,
}

impl<'a> RowSink<'a> {
    /// Sends the schema. Returns `None` if the consumer went away.
    pub async fn start(cols: &[Column], batch: usize, tx: &'a StreamSender) -> Option<RowSink<'a>> {
        let builder = BatchBuilder::new(cols, batch.max(1));
        if !tx.send(StreamEvent::Schema(builder.schema())).await {
            return None;
        }
        Some(RowSink { builder, types: cols.iter().map(|c| c.col_type).collect(), tx, rows: 0 })
    }

    /// Push a row; returns `false` when the consumer stopped.
    pub async fn push(&mut self, row: impl IntoIterator<Item = Value>) -> Result<bool> {
        self.builder.push_row(row);
        self.rows += 1;
        if self.builder.is_full() {
            return Ok(self.tx.send(StreamEvent::Batch(self.builder.finish()?)).await);
        }
        Ok(true)
    }

    pub async fn finish(mut self) -> Result<()> {
        if !self.builder.is_empty() && !self.tx.send(StreamEvent::Batch(self.builder.finish()?)).await {
            return Ok(());
        }
        if self.builder.coercion_failures() > 0 {
            self.tx
                .send(StreamEvent::Notice(format!(
                    "{} value(s) could not be converted and are shown as NULL",
                    self.builder.coercion_failures()
                )))
                .await;
        }
        Ok(())
    }
}

/// Exponential backoff for statement polling (100 ms → 2 s).
pub struct Backoff(u64);

impl Backoff {
    pub fn new() -> Self {
        Self(100)
    }
    pub async fn wait(&mut self) {
        tokio::time::sleep(Duration::from_millis(self.0)).await;
        self.0 = (self.0 * 2).min(2000);
    }
}

pub fn normalize_host(h: &str) -> String {
    h.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string()
}

/// Results above this size are "large" (Databricks' INLINE limit, 25 MiB).
pub const LARGE_RESULT_BYTES: u64 = 25 * 1024 * 1024;

/// `1.4 GB`, `820 MB`, `12 KB`.
pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else if v < 10.0 { format!("{v:.1} {}", U[i]) } else { format!("{v:.0} {}", U[i]) }
}

/// A presigned link without its query string (the signature): safe to show.
pub fn link_without_signature(url: &str) -> String {
    match url.find('?') {
        Some(i) => format!("{}?…", &url[..i]),
        None => url.to_string(),
    }
}

/// Notice for a result downloaded in chunks from cloud storage, or `None`
/// when it is small. `bytes`/`rows` may be unknown.
pub fn large_result_notice(engine: &str, bytes: Option<u64>, rows: Option<u64>, chunks: u64, link: Option<&str>) -> Option<String> {
    let large = match bytes {
        Some(b) => b > LARGE_RESULT_BYTES,
        None => chunks > 1,
    };
    if !large {
        return None;
    }
    let mut size = Vec::new();
    if let Some(b) = bytes {
        size.push(human_bytes(b));
    }
    if let Some(r) = rows {
        size.push(format!("{r} rows"));
    }
    let what = if size.is_empty() { format!("{chunks} chunks") } else { format!("{} in {chunks} chunks", size.join(", ")) };
    let from = link.map(|l| format!(": {}", link_without_signature(l))).unwrap_or_default();
    Some(format!(
        "The data is large ({what}), so it is downloaded from {engine}'s external links{from}. The query has already finished on the server; the rest is the download, which can take a while."
    ))
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    #[test]
    fn large_result_notices() {
        assert_eq!(human_bytes(1_400_000_000), "1.4 GB");
        assert_eq!(human_bytes(820_000_000), "820 MB");
        assert_eq!(large_result_notice("Databricks", Some(1000), Some(3), 1, None), None);
        assert_eq!(large_result_notice("Databricks", None, None, 1, None), None);
        let n = large_result_notice("Databricks", Some(1_400_000_000), Some(3_200_000), 52, Some("https://st.blob.core.windows.net/r/0?sig=SECRET&se=1")).unwrap();
        assert!(n.starts_with("The data is large (1.4 GB, 3200000 rows in 52 chunks), so it is downloaded from Databricks's external links: https://st.blob.core.windows.net/r/0?…."), "{n}");
        assert!(!n.contains("SECRET"));
        assert!(large_result_notice("Snowflake", None, None, 4, None).unwrap().contains("(4 chunks)"));
    }
}
