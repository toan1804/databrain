//! Session state, authenticated HTTP requests and the statement lifecycle
//! (submit, poll, small result sets).

use std::sync::Arc;

use databrain_auth::{Credential, CredentialSource};
use databrain_connector_core::{ConnectorError, Result};
use secrecy::ExposeSecret;
use serde_json::json;
use tokio::sync::Mutex;

use crate::common::{Backoff, json_body, net_err};

use super::download::first_link;
use super::stream::{Disposition, is_inline_limit};

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

    /// Wait until the statement finishes; returns the final response.
    pub(super) async fn wait(&self, mut resp: serde_json::Value, cancel: &databrain_connector_core::CancellationToken) -> Result<serde_json::Value> {
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
                _ => {}
            }
            tokio::select! {
                _ = backoff.wait() => {}
                _ = cancel.cancelled() => {
                    let _ = self.request(reqwest::Method::POST, format!("{}/{id}/cancel", self.base), None).await;
                    return Err(ConnectorError::cancelled());
                }
            }
            resp = self.request(reqwest::Method::GET, format!("{}/{id}", self.base), None).await?;
        }
    }

    pub(super) async fn run_small(&self, sql: &str) -> Result<Vec<Vec<serde_json::Value>>> {
        let cancel = databrain_connector_core::CancellationToken::new();
        let resp = self.submit(sql, Some(100_000)).await?;
        let resp = match self.wait(resp, &cancel).await {
            // Metadata of a huge catalog can pass 25 MiB: fetch it through links.
            Err(e) if is_inline_limit(&e.message) && !self.inline_only => {
                let resp = self.submit_as(sql, Some(100_000), Disposition::ExternalLinks).await?;
                self.wait(resp, &cancel).await?
            }
            r => r?,
        };
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
