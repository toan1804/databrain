//! Session state, authenticated HTTP requests and the statement lifecycle
//! (submit, poll, small result sets).

use std::sync::Arc;

use databrain_auth::{Credential, CredentialSource};
use databrain_connector_core::{ConnectorError, Result, StreamSender};
use secrecy::ExposeSecret;
use serde_json::json;
use tokio::sync::Mutex;

use crate::common::{Backoff, json_body, net_err};

use super::download::first_link;
use super::stream::{Disposition, is_inline_limit};

/// Most rows a metadata query ([`Inner::run_small`]) reads.
pub(super) const SMALL_ROWS: usize = 100_000;

pub struct DbxSession(pub(super) Arc<Inner>);

pub(super) struct Inner {
    /// `https://<host>`, joined with the API's chunk links.
    pub(super) origin: String,
    pub(super) base: String,
    pub(super) warehouse: String,
    pub(super) creds: Arc<dyn CredentialSource>,
    pub(super) http: reqwest::Client,
    /// Session context emulated client-side (the API is stateless): `USE` updates these.
    pub(super) catalog: Mutex<Option<String>>,
    pub(super) schema: Mutex<Option<String>>,
    /// "Result transfer: Inline" option: never use EXTERNAL_LINKS.
    pub(super) inline_only: bool,
    /// Result chunks downloaded at once.
    pub(super) fetch_parallel: usize,
    /// Client for presigned cloud-storage downloads.
    pub(super) storage: reqwest::Client,
    /// Volume folder bulk loads stage their files in (none: loads use INSERTs).
    pub(super) staging: Option<String>,
}

impl Inner {
    pub(super) async fn token(&self) -> Result<String> {
        match self.creds.get().await? {
            Credential::Bearer { token, .. } => Ok(token.expose_secret().to_string()),
            _ => Err(ConnectorError::config("Databricks requires a token or OAuth sign-in")),
        }
    }

    pub(super) async fn request(&self, method: reqwest::Method, url: String, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
        for attempt in 0..2 {
            let token = self.token().await?;
            let mut req = self.http.request(method.clone(), &url).bearer_auth(token);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(net_err)?;
            if resp.status().as_u16() == 401 && attempt == 0 {
                self.creds.invalidate().await;
                continue;
            }
            return json_body(resp, "Databricks").await;
        }
        unreachable!()
    }

    pub(super) async fn submit(&self, sql: &str, row_limit: Option<usize>) -> Result<serde_json::Value> {
        self.submit_as(sql, row_limit, Disposition::Inline).await
    }

    pub(super) async fn submit_as(&self, sql: &str, row_limit: Option<usize>, disposition: Disposition) -> Result<serde_json::Value> {
        let mut body = json!({
            "statement": sql,
            "warehouse_id": self.warehouse,
            "wait_timeout": "10s",
            "on_wait_timeout": "CONTINUE",
            "disposition": disposition.as_str(),
            "format": disposition.format(),
        });
        if let Some(c) = self.catalog.lock().await.clone() {
            body["catalog"] = c.into();
        }
        if let Some(s) = self.schema.lock().await.clone() {
            body["schema"] = s.into();
        }
        if let Some(l) = row_limit {
            body["row_limit"] = (l as u64).into();
        }
        self.request(reqwest::Method::POST, self.base.clone(), Some(body)).await
    }

    /// Cancel a statement on the server, best effort.
    async fn cancel_statement(&self, id: &str) {
        let _ = self.request(reqwest::Method::POST, format!("{}/{id}/cancel", self.base), None).await;
    }

    /// Wait until the statement finishes; returns the final response. When
    /// waiting fails in a way that may leave it running (the connection
    /// broke, an unexpected answer), or nobody waits for it any more (`tx`
    /// dropped), the statement is cancelled on the server before returning.
    pub(super) async fn wait(
        &self,
        mut resp: serde_json::Value,
        cancel: &databrain_connector_core::CancellationToken,
        tx: Option<&StreamSender>,
    ) -> Result<serde_json::Value> {
        let id = resp["statement_id"].as_str().unwrap_or_default().to_string();
        let mut backoff = Backoff::new();
        loop {
            match resp.pointer("/status/state").and_then(|s| s.as_str()).unwrap_or("") {
                "SUCCEEDED" => return Ok(resp),
                "FAILED" | "CLOSED" => {
                    let msg = resp.pointer("/status/error/message").and_then(|m| m.as_str()).unwrap_or("statement failed");
                    let code = resp.pointer("/status/error/error_code").and_then(|m| m.as_str()).unwrap_or("");
                    return Err(ConnectorError::query(msg.to_string()).with_code(code.to_string()));
                }
                "CANCELED" => return Err(ConnectorError::cancelled()),
                "PENDING" | "RUNNING" => {}
                // Not an answer about the statement: it may still run.
                _ => {
                    if !id.is_empty() {
                        self.cancel_statement(&id).await;
                    }
                    return Err(ConnectorError::connection(format!(
                        "Databricks: unexpected answer while waiting for the statement: {}",
                        crate::common::truncate(&resp.to_string(), 300)
                    )));
                }
            }
            tokio::select! {
                _ = backoff.wait() => {}
                _ = cancel.cancelled() => {}
            }
            if crate::common::abandoned(cancel, tx) {
                self.cancel_statement(&id).await;
                return Err(ConnectorError::cancelled());
            }
            resp = match self.request(reqwest::Method::GET, format!("{}/{id}", self.base), None).await {
                Ok(r) => r,
                Err(e) => {
                    if crate::common::may_still_run(&e) {
                        self.cancel_statement(&id).await;
                    }
                    return Err(e);
                }
            };
        }
    }

    /// Rows of a metadata query, at most [`SMALL_ROWS`]. Past that it fails
    /// rather than returning part of the list as if it were all of it: a
    /// catalog's bulk read then goes schema by schema.
    pub(super) async fn run_small(&self, sql: &str) -> Result<Vec<Vec<serde_json::Value>>> {
        let cancel = databrain_connector_core::CancellationToken::new();
        let resp = self.submit(sql, Some(SMALL_ROWS)).await?;
        let resp = match self.wait(resp, &cancel, None).await {
            // Metadata of a huge catalog can pass 25 MiB: fetch it through links.
            Err(e) if is_inline_limit(&e.message) && !self.inline_only => {
                let resp = self.submit_as(sql, Some(SMALL_ROWS), Disposition::ExternalLinks).await?;
                self.wait(resp, &cancel, None).await?
            }
            r => r?,
        };
        if resp.pointer("/manifest/truncated").and_then(|t| t.as_bool()) == Some(true) {
            return Err(ConnectorError::query(format!(
                "Databricks returned only the first {SMALL_ROWS} rows of this metadata query; the list would be incomplete"
            )));
        }
        let id = resp["statement_id"].as_str().unwrap_or_default().to_string();
        let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
        let mut chunk = resp.get("result").cloned();
        while let Some(c) = chunk {
            rows.extend(self.chunk_rows(&id, &c).await?.iter().map(|r| r.as_array().cloned().unwrap_or_default()));
            chunk = self.next_chunk(&c).await?;
        }
        Ok(rows)
    }

    /// Rows of a result chunk: inline `data_array`, or downloaded from its external link.
    pub(super) async fn chunk_rows(&self, statement_id: &str, chunk: &serde_json::Value) -> Result<Vec<serde_json::Value>> {
        match first_link(chunk) {
            Some(link) => self.download(statement_id, &link).await,
            None => Ok(chunk["data_array"].as_array().cloned().unwrap_or_default()),
        }
    }

    /// Next chunk after `chunk`, if any (EXTERNAL_LINKS puts the link on the link entry).
    pub(super) async fn next_chunk(&self, chunk: &serde_json::Value) -> Result<Option<serde_json::Value>> {
        let next = chunk.get("next_chunk_internal_link").or_else(|| chunk.pointer("/external_links/0/next_chunk_internal_link")).and_then(|l| l.as_str());
        match next {
            Some(link) => Ok(Some(self.request(reqwest::Method::GET, format!("{}{link}", self.origin), None).await?)),
            None => Ok(None),
        }
    }

    pub(super) async fn apply_use(&self, sql: &str) {
        let words: Vec<&str> = sql.split_whitespace().collect();
        let (kind, target) = match words.as_slice() {
            [_, k, t, ..] if k.eq_ignore_ascii_case("catalog") => ("catalog", *t),
            [_, k, t, ..] if k.eq_ignore_ascii_case("schema") || k.eq_ignore_ascii_case("database") => ("schema", *t),
            [_, t, ..] => ("schema", *t),
            _ => return,
        };
        let t = target.trim_end_matches(';').replace('`', "");
        if kind == "catalog" {
            *self.catalog.lock().await = Some(t);
        } else if let Some((c, s)) = t.split_once('.') {
            *self.catalog.lock().await = Some(c.to_string());
            *self.schema.lock().await = Some(s.to_string());
        } else {
            *self.schema.lock().await = Some(t);
        }
    }
}

#[cfg(test)]
mod poll_tests {
    use super::*;
    use databrain_connector_core::{CancellationToken, ErrorKind, QueryStream};

    /// A statement still running; each poll gets `poll` (status 0: the connection drops).
    async fn running(poll: (u16, &'static str)) -> (Arc<Inner>, crate::mock_http::Log) {
        let (origin, log) = crate::mock_http::serve(move |m, p, _| {
            if m == "GET" && p == "/api/2.0/sql/statements/s9" {
                (poll.0, poll.1.to_string())
            } else if m == "POST" && p == "/api/2.0/sql/statements/s9/cancel" {
                (200, "{}".into())
            } else {
                (404, "{}".into())
            }
        })
        .await;
        (super::super::testutil::mock::session(&origin, false).0, log)
    }

    fn cancels(log: &crate::mock_http::Log) -> usize {
        log.lock().unwrap().iter().filter(|(m, p, _)| m == "POST" && p.ends_with("/s9/cancel")).count()
    }

    fn in_progress() -> serde_json::Value {
        json!({"statement_id": "s9", "status": {"state": "RUNNING"}})
    }

    /// A poll that fails without the server saying how the statement ended: it is cancelled.
    #[tokio::test]
    async fn a_failed_poll_cancels_the_statement() {
        for poll in [(0, ""), (502, "<html>Bad gateway</html>"), (200, r#"{"weird": 1}"#)] {
            let (i, log) = running(poll).await;
            let err = i.wait(in_progress(), &CancellationToken::new(), None).await.unwrap_err();
            assert!(err.kind != ErrorKind::Query, "{poll:?}: {err:?}");
            assert_eq!(cancels(&log), 1, "{poll:?}");
        }
    }

    /// The server said how the statement ended (failed, done, or gone): nothing to cancel.
    #[tokio::test]
    async fn a_statement_that_ended_is_not_cancelled() {
        let failed: &'static str = r#"{"statement_id": "s9", "status": {"state": "FAILED", "error": {"message": "TABLE_OR_VIEW_NOT_FOUND"}}}"#;
        let done: &'static str = r#"{"statement_id": "s9", "status": {"state": "SUCCEEDED"}}"#;
        let missing: &'static str = r#"{"error_code": "NOT_FOUND", "message": "statement s9 not found"}"#;
        for (poll, ok) in [((200, failed), false), ((200, done), true), ((404, missing), false)] {
            let (i, log) = running(poll).await;
            assert_eq!(i.wait(in_progress(), &CancellationToken::new(), None).await.is_ok(), ok, "{poll:?}");
            assert_eq!(cancels(&log), 0, "{poll:?}");
        }
    }

    /// Nobody reads the rows any more (the stream dropped): the statement is cancelled.
    #[tokio::test]
    async fn a_dropped_stream_cancels_the_statement() {
        let (i, log) = running((200, r#"{"statement_id": "s9", "status": {"state": "RUNNING"}}"#)).await;
        let (tx, stream) = QueryStream::channel(1);
        drop(stream);
        let err = i.wait(in_progress(), &CancellationToken::new(), Some(&tx)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);
        assert_eq!(cancels(&log), 1);
    }
}
