//! Downloading result chunks from presigned cloud-storage links: link parsing,
//! retries with backoff and link renewal.

use databrain_connector_core::{ConnectorError, Result};

use super::client::Inner;

#[derive(Clone)]
pub(super) struct ChunkLink {
    pub(super) url: String,
    pub(super) chunk_index: u64,
    /// When the presigned URL stops working (from the API's `expiration`).
    pub(super) expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ChunkLink {
    /// Expired or about to: renew before use (links live ≤ 15 minutes, and a
    /// download started at the last second can still fail).
    pub(super) fn expiring(&self) -> bool {
        self.expires_at.is_some_and(|t| t - chrono::Utc::now() < chrono::Duration::seconds(LINK_MARGIN_SECS))
    }
}

pub(super) fn parse_link(l: &serde_json::Value) -> Option<ChunkLink> {
    Some(ChunkLink {
        url: l["external_link"].as_str()?.to_string(),
        chunk_index: l["chunk_index"].as_u64().unwrap_or(0),
        expires_at: l["expiration"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&chrono::Utc)),
    })
}

/// First external link of a result chunk (EXTERNAL_LINKS disposition).
pub(super) fn first_link(chunk: &serde_json::Value) -> Option<ChunkLink> {
    parse_link(chunk.pointer("/external_links/0")?)
}

/// Attempts per chunk download (network errors, throttling, 5xx).
pub(super) const DOWNLOAD_ATTEMPTS: u32 = 7;
/// Renewals of an expired presigned link per chunk.
pub(super) const MAX_LINK_REFRESHES: u32 = 3;
/// Renew a link this many seconds before it expires.
pub(super) const LINK_MARGIN_SECS: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Retry {
    /// Network error, timeout, 408/429/5xx: back off and retry.
    Transient,
    /// 400/403/404 from storage: the presigned link expired; renew it.
    Expired,
    Fatal,
}

pub(super) struct FetchFail {
    pub(super) class: Retry,
    pub(super) retry_after: Option<std::time::Duration>,
    pub(super) message: String,
}

/// Backoff before retry `attempt` (0-based): 250 ms × 2ⁿ capped at 8 s, with
/// ±50% jitter so parallel downloads don't retry in lockstep. Retry-After wins.
pub(super) fn backoff_delay(attempt: u32, retry_after: Option<std::time::Duration>) -> std::time::Duration {
    if let Some(d) = retry_after {
        return d;
    }
    // Tests use tiny delays.
    let unit = if cfg!(test) { 2 } else { 250 };
    let base = (unit << attempt.min(5)).min(unit * 32);
    // Cheap jitter without a RNG dependency.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0);
    let jitter = nanos % (base + 1);
    std::time::Duration::from_millis(base / 2 + jitter)
}

pub(super) async fn backoff_sleep(attempt: u32, retry_after: Option<std::time::Duration>) {
    tokio::time::sleep(backoff_delay(attempt, retry_after)).await;
}

/// Drop query strings from URLs in error text (presigned URLs carry credentials).
pub(super) fn redact_url(s: &str) -> String {
    s.split_whitespace()
        .map(|w| match w.find('?') {
            Some(i) if w.contains("://") => format!("{}?…", &w[..i]),
            _ => w.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Inner {
    /// Bytes of result chunk `i`, with retries:
    /// - network errors, timeouts, 408/429/5xx and broken bodies are retried
    ///   with exponential backoff and jitter (Retry-After is honored);
    /// - expired presigned links (400/403/404, or past their `expiration`) are
    ///   renewed through the chunk API, then retried.
    pub(super) async fn chunk_bytes(&self, statement_id: &str, i: u64, link: Option<ChunkLink>) -> Result<Vec<u8>> {
        let mut link = link;
        let mut refreshes = 0u32;
        let mut last: Option<String> = None;
        for attempt in 0..DOWNLOAD_ATTEMPTS {
            // Missing, expired or about to expire: ask for a fresh link.
            if link.as_ref().is_none_or(|l| l.expiring()) {
                if statement_id.is_empty() && link.is_some() {
                    // Nothing to renew from; try the link we have.
                } else {
                    match self.resolve_link(statement_id, i).await {
                        Ok(l) => link = Some(l),
                        Err(e) if e.kind == databrain_connector_core::ErrorKind::Connection && attempt + 1 < DOWNLOAD_ATTEMPTS => {
                            last = Some(e.message);
                            backoff_sleep(attempt, None).await;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            let l = link.as_ref().ok_or_else(|| ConnectorError::internal(format!("result chunk {i} has no download link")))?;
            match self.get_bytes(&l.url).await {
                Ok(b) => return Ok(b),
                Err(f) => {
                    last = Some(f.message.clone());
                    match f.class {
                        Retry::Expired if refreshes < MAX_LINK_REFRESHES && !statement_id.is_empty() => {
                            refreshes += 1;
                            link = None; // renewed at the top of the loop, no wait
                        }
                        Retry::Transient if attempt + 1 < DOWNLOAD_ATTEMPTS => backoff_sleep(attempt, f.retry_after).await,
                        _ => break,
                    }
                }
            }
        }
        Err(ConnectorError::connection(format!("result chunk {i} download failed after retries: {}", last.unwrap_or_else(|| "unknown error".into()))))
    }

    /// Fresh download link for chunk `i` from the statement API.
    pub(super) async fn resolve_link(&self, statement_id: &str, i: u64) -> Result<ChunkLink> {
        let c = self.request(reqwest::Method::GET, format!("{}/{statement_id}/result/chunks/{i}", self.base), None).await?;
        first_link(&c).ok_or_else(|| ConnectorError::internal(format!("result chunk {i} has no download link")))
    }

    /// One GET of a presigned URL (no Authorization header; the URL is never logged).
    pub(super) async fn get_bytes(&self, url: &str) -> std::result::Result<Vec<u8>, FetchFail> {
        let net = |e: reqwest::Error| FetchFail { class: Retry::Transient, retry_after: None, message: redact_url(&crate::common::error_chain(&e)) };
        let resp = self.storage.get(url).send().await.map_err(net)?;
        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|s| std::time::Duration::from_secs(s.min(30)));
            let class = match code {
                400 | 403 | 404 => Retry::Expired,
                408 | 429 | 500..=599 => Retry::Transient,
                _ => Retry::Fatal,
            };
            return Err(FetchFail { class, retry_after, message: format!("HTTP {status}") });
        }
        // A body cut off mid-way is retried like any network error.
        resp.bytes().await.map(|b| b.to_vec()).map_err(net)
    }

    /// Fetch one EXTERNAL_LINKS chunk (JSON_ARRAY rows). The URL is a presigned
    /// cloud-storage link: no Authorization header, and it is never logged.
    /// An expired link is renewed once through the chunk API.
    pub(super) async fn download(&self, statement_id: &str, link: &ChunkLink) -> Result<Vec<serde_json::Value>> {
        let bytes = self.chunk_bytes(statement_id, link.chunk_index, Some(link.clone())).await?;
        serde_json::from_slice(&bytes).map_err(|e| ConnectorError::internal(format!("unexpected result chunk: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::testutil::mock;
    use super::*;

    #[tokio::test]
    async fn give_up_with_a_clear_error() {
        let (origin, log) = mock::serve("always_503").await;
        let s = mock::session(&origin, true);
        let e = s.0.chunk_bytes("st1", 2, None).await.unwrap_err();
        assert!(e.message.contains("chunk 2") && e.message.contains("after retries") && e.message.contains("503"), "{}", e.message);
        assert_eq!(log.lock().unwrap().hits.values().sum::<u32>(), DOWNLOAD_ATTEMPTS);
        let (origin, log) = mock::serve("always_403").await;
        let e = mock::session(&origin, true).0.chunk_bytes("st1", 1, None).await.unwrap_err();
        assert!(e.message.contains("403"), "{}", e.message);
        assert_eq!(log.lock().unwrap().hits.values().sum::<u32>(), MAX_LINK_REFRESHES + 1, "renewed a few times, then stopped");
    }

    #[test]
    fn backoff_grows_and_honors_retry_after() {
        assert_eq!(backoff_delay(3, Some(std::time::Duration::from_secs(4))), std::time::Duration::from_secs(4));
        for a in 0..10 {
            let d = backoff_delay(a, None).as_millis() as u64;
            let base = (2u64 << a.min(5)).min(64);
            assert!(d >= base / 2 && d <= base / 2 + base, "attempt {a}: {d}");
        }
        let past = json!({"external_link": "https://x", "chunk_index": 1, "expiration": "2020-01-01T00:00:00Z"});
        assert!(parse_link(&past).unwrap().expiring());
        let future = json!({"external_link": "https://x", "chunk_index": 1, "expiration": (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339()});
        assert!(!parse_link(&future).unwrap().expiring());
        assert!(!parse_link(&json!({"external_link": "https://x"})).unwrap().expiring(), "no expiration: use as is");
    }
}
