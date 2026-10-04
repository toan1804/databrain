//! Databricks SQL via the Statement Execution API 2.0.
//!
//! Auth: personal access token, OAuth U2M (browser + PKCE against the
//! workspace's `/oidc` endpoints with the `databricks-cli` public client),
//! OAuth M2M (service principal) and existing Databricks CLI profiles.
//! Names are three-level; schemas are exposed as `catalog.schema`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use databrain_auth::{AuthContext, AuthMethod, AuthMethodKind, CliKind, ClientAuthStyle, Credential, CredentialSource, OAuthConfig};
use databrain_connector_core::arrow::array::{Array, RecordBatch};
use databrain_connector_core::arrow::datatypes::SchemaRef;
use databrain_connector_core::value::Column;
use databrain_connector_core::{
    Capabilities, ColType, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind,
    DbObject, ExecOptions, ExecSummary, FieldSpec, ObjectDetail, ObjectKind, QueryStream, Result,
    SchemaInfo, SchemaMetadata, Session, StreamEvent, StreamSender, TableColumns, quote_ident, quote_literal,
};
use secrecy::ExposeSecret;
use serde_json::json;
use tokio::sync::Mutex;

use crate::common::{Backoff, RowSink, http, json_body, json_cell, net_err, normalize_host};

/// Public OAuth client registered by Databricks for CLI/SDK tools.
pub const DEFAULT_CLIENT_ID: &str = "databricks-cli";
const DEFAULT_REDIRECT_PORT: u16 = 8020;

#[derive(Debug, Default)]
pub struct DatabricksConnector;

impl DatabricksConnector {
    pub fn new() -> Self {
        Self
    }
}

fn warehouse_id(cfg: &ConnectionConfig) -> Result<String> {
    if let Some(id) = cfg.opt("warehouse_id") {
        return Ok(id.to_string());
    }
    let path = cfg
        .opt("http_path")
        .ok_or_else(|| ConnectorError::config("Databricks needs the SQL warehouse HTTP path (e.g. /sql/1.0/warehouses/abc123)"))?;
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ConnectorError::config("invalid HTTP path"))
}

#[async_trait]
impl Connector for DatabricksConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Databricks,
            display_name: "Databricks",
            default_port: None,
            uses_file: false,
            auth_methods: vec![
                AuthMethodKind::OauthBrowser,
                AuthMethodKind::ApiToken,
                AuthMethodKind::ClientCredentials,
                AuthMethodKind::CloudCli,
            ],
            capabilities: Capabilities { transactions: false, cancel: true, schemas: true, read_only_sessions: false, ssh: false },
            fields: vec![
                FieldSpec::new("host", "Workspace host").required().placeholder("adb-1234567890.12.azuredatabricks.net"),
                FieldSpec::new("http_path", "SQL warehouse HTTP path").required().placeholder("/sql/1.0/warehouses/abc123def456"),
                FieldSpec::new("catalog", "Default catalog").placeholder("main"),
                FieldSpec::new("schema", "Default schema").placeholder("default"),
                FieldSpec::new("fetch_parallel", "Parallel downloads").placeholder("8").help("Result chunks downloaded at the same time (1-32)."),
                FieldSpec::new("result_transfer", "Result transfer")
                    .placeholder("auto")
                    .help("auto: large results download from cloud storage (no 25 MiB limit). inline: only through the workspace API (25 MiB limit), for networks that block cloud storage."),
            ],
            note: Some("Browser sign-in uses the Databricks CLI OAuth app (redirect http://localhost:8020)."),
        }
    }

    fn auth_context(&self, cfg: &ConnectionConfig) -> AuthContext {
        let host = normalize_host(cfg.host.as_deref().unwrap_or_default());
        let p = cfg.auth.oauth_params().cloned().unwrap_or_default();
        let m2m = matches!(cfg.auth, AuthMethod::ClientCredentials(_));
        let scopes = p
            .scopes
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|| if m2m { vec!["all-apis".into()] } else { vec!["sql".into(), "offline_access".into()] });
        AuthContext {
            oauth: Some(OAuthConfig {
                provider: "Databricks".into(),
                authorize_url: format!("https://{host}/oidc/v1/authorize"),
                token_url: format!("https://{host}/oidc/v1/token"),
                device_url: None,
                revoke_url: None,
                client_id: p.client_id.unwrap_or_else(|| DEFAULT_CLIENT_ID.into()),
                client_secret: None,
                scopes,
                redirect_host: "localhost".into(),
                redirect_port: Some(p.redirect_port.unwrap_or(DEFAULT_REDIRECT_PORT)),
                redirect_path: "/".into(),
                extra_authorize_params: vec![],
                client_auth: if m2m { ClientAuthStyle::Basic } else { ClientAuthStyle::Body },
            }),
            cli: Some(CliKind::Databricks { host }),
            google_scopes: vec![],
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let host = normalize_host(cfg.host.as_deref().filter(|h| !h.is_empty()).ok_or_else(|| ConnectorError::config("workspace host is required"))?);
        let s = DbxSession(Arc::new(Inner {
            base: format!("https://{host}/api/2.0/sql/statements"),
            origin: format!("https://{host}"),
            warehouse: warehouse_id(cfg)?,
            creds,
            http: http(),
            catalog: Mutex::new(cfg.opt("catalog").map(str::to_string)),
            schema: Mutex::new(cfg.opt("schema").map(str::to_string)),
            inline_only: cfg.opt("result_transfer").is_some_and(|v| v.eq_ignore_ascii_case("inline")),
            storage: crate::common::storage_http(),
            fetch_parallel: cfg.opt("fetch_parallel").and_then(|v| v.parse().ok()).filter(|n: &usize| (1..=32).contains(n)).unwrap_or(FETCH_PARALLEL),
        }));
        // Validate credentials + warehouse up front.
        s.0.run_small("SELECT 1").await?;
        Ok(Box::new(s))
    }
}

pub struct DbxSession(Arc<Inner>);

struct Inner {
    /// `https://<host>`, joined with the API's chunk links.
    origin: String,
    base: String,
    warehouse: String,
    creds: Arc<dyn CredentialSource>,
    http: reqwest::Client,
    /// Session context emulated client-side (the API is stateless): `USE` updates these.
    catalog: Mutex<Option<String>>,
    schema: Mutex<Option<String>>,
    /// "Result transfer: Inline" option: never use EXTERNAL_LINKS.
    inline_only: bool,
    /// Result chunks downloaded at once.
    fetch_parallel: usize,
    /// Client for presigned cloud-storage downloads.
    storage: reqwest::Client,
}

fn col_type(type_name: &str) -> ColType {
    match type_name.to_ascii_uppercase().as_str() {
        "BOOLEAN" => ColType::Bool,
        "BYTE" | "TINYINT" | "SHORT" | "SMALLINT" | "INT" | "INTEGER" | "LONG" | "BIGINT" => ColType::Int64,
        "FLOAT" | "DOUBLE" => ColType::Float64,
        "DATE" => ColType::Date,
        "TIMESTAMP" => ColType::TimestampTz,
        "TIMESTAMP_NTZ" => ColType::Timestamp,
        "BINARY" => ColType::Utf8,
        _ => ColType::Utf8, // DECIMAL kept exact, complex types as JSON text
    }
}

impl Inner {
    async fn token(&self) -> Result<String> {
        match self.creds.get().await? {
            Credential::Bearer { token, .. } => Ok(token.expose_secret().to_string()),
            _ => Err(ConnectorError::config("Databricks requires a token or OAuth sign-in")),
        }
    }

    async fn request(&self, method: reqwest::Method, url: String, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
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

    async fn submit(&self, sql: &str, row_limit: Option<usize>) -> Result<serde_json::Value> {
        self.submit_as(sql, row_limit, Disposition::Inline).await
    }

    async fn submit_as(&self, sql: &str, row_limit: Option<usize>, disposition: Disposition) -> Result<serde_json::Value> {
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
    async fn wait(&self, mut resp: serde_json::Value, cancel: &databrain_connector_core::CancellationToken) -> Result<serde_json::Value> {
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

    async fn run_small(&self, sql: &str) -> Result<Vec<Vec<serde_json::Value>>> {
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
    async fn chunk_rows(&self, statement_id: &str, chunk: &serde_json::Value) -> Result<Vec<serde_json::Value>> {
        match first_link(chunk) {
            Some(link) => self.download(statement_id, &link).await,
            None => Ok(chunk["data_array"].as_array().cloned().unwrap_or_default()),
        }
    }

    /// Next chunk after `chunk`, if any (EXTERNAL_LINKS puts the link on the link entry).
    async fn next_chunk(&self, chunk: &serde_json::Value) -> Result<Option<serde_json::Value>> {
        let next = chunk
            .get("next_chunk_internal_link")
            .or_else(|| chunk.pointer("/external_links/0/next_chunk_internal_link"))
            .and_then(|l| l.as_str());
        match next {
            Some(link) => Ok(Some(self.request(reqwest::Method::GET, format!("{}{link}", self.origin), None).await?)),
            None => Ok(None),
        }
    }

    async fn stream(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
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
    async fn run_statement(&self, sql: &str, opts: &ExecOptions, disposition: Disposition) -> std::result::Result<serde_json::Value, StreamFail> {
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
    async fn stream_arrow(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> std::result::Result<(), StreamFail> {
        use futures::StreamExt as _;
        let resp = self.run_statement(sql, opts, Disposition::ArrowLinks).await?;
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
        let total = resp
            .pointer("/manifest/total_chunk_count")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| known.keys().max().map(|m| m + 1).unwrap_or(0));
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
                tokio::task::spawn_blocking(move || decode_arrow(&bytes, &schema))
                    .await
                    .map_err(|e| ConnectorError::internal(e.to_string()))?
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

    /// Bytes of result chunk `i`, with retries:
    /// - network errors, timeouts, 408/429/5xx and broken bodies are retried
    ///   with exponential backoff and jitter (Retry-After is honored);
    /// - expired presigned links (400/403/404, or past their `expiration`) are
    ///   renewed through the chunk API, then retried.
    async fn chunk_bytes(&self, statement_id: &str, i: u64, link: Option<ChunkLink>) -> Result<Vec<u8>> {
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
        Err(ConnectorError::connection(format!(
            "result chunk {i} download failed after retries: {}",
            last.unwrap_or_else(|| "unknown error".into())
        )))
    }

    /// Fresh download link for chunk `i` from the statement API.
    async fn resolve_link(&self, statement_id: &str, i: u64) -> Result<ChunkLink> {
        let c = self.request(reqwest::Method::GET, format!("{}/{statement_id}/result/chunks/{i}", self.base), None).await?;
        first_link(&c).ok_or_else(|| ConnectorError::internal(format!("result chunk {i} has no download link")))
    }

    /// One GET of a presigned URL (no Authorization header; the URL is never logged).
    async fn get_bytes(&self, url: &str) -> std::result::Result<Vec<u8>, FetchFail> {
        let net = |e: reqwest::Error| FetchFail {
            class: Retry::Transient,
            retry_after: None,
            message: redact_url(&crate::common::error_chain(&e)),
        };
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

    async fn stream_as(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender, disposition: Disposition) -> std::result::Result<(), StreamFail> {
        let resp = self.run_statement(sql, opts, disposition).await?;
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
        if let Some(c) = &chunk {
            if let Some(link) = first_link(c) {
                match self.download(&statement_id, &link).await {
                    Ok(rows) => pending = Some(rows),
                    Err(e) => return Err(StreamFail::Links(e)),
                }
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

    /// Fetch one EXTERNAL_LINKS chunk (JSON_ARRAY rows). The URL is a presigned
    /// cloud-storage link: no Authorization header, and it is never logged.
    /// An expired link is renewed once through the chunk API.
    async fn download(&self, statement_id: &str, link: &ChunkLink) -> Result<Vec<serde_json::Value>> {
        let bytes = self.chunk_bytes(statement_id, link.chunk_index, Some(link.clone())).await?;
        serde_json::from_slice(&bytes).map_err(|e| ConnectorError::internal(format!("unexpected result chunk: {e}")))
    }

    async fn apply_use(&self, sql: &str) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Inline,
    /// EXTERNAL_LINKS with JSON_ARRAY chunks (fallback when Arrow is refused).
    ExternalLinks,
    /// EXTERNAL_LINKS with ARROW_STREAM chunks, downloaded in parallel (default).
    ArrowLinks,
}

impl Disposition {
    fn as_str(self) -> &'static str {
        match self {
            Disposition::Inline => "INLINE",
            Disposition::ExternalLinks | Disposition::ArrowLinks => "EXTERNAL_LINKS",
        }
    }
    fn format(self) -> &'static str {
        match self {
            Disposition::ArrowLinks => "ARROW_STREAM",
            _ => "JSON_ARRAY",
        }
    }
}

/// Result chunks downloaded at the same time (like the Python connector's
/// CloudFetch). Overridable per connection with the `fetch_parallel` option.
const FETCH_PARALLEL: usize = 8;

/// Why a streamed statement failed: external links could not be used
/// (retry inline), or anything else.
enum StreamFail {
    Links(ConnectorError),
    /// The server refused ARROW_STREAM: retry with JSON chunks.
    Format(ConnectorError),
    Other(ConnectorError),
}

impl StreamFail {
    fn into_inner(self) -> ConnectorError {
        match self {
            StreamFail::Links(e) | StreamFail::Format(e) | StreamFail::Other(e) => e,
        }
    }
}

/// Server errors about the requested result format.
fn mentions_format(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("arrow_stream") || m.contains("arrow stream") || (m.contains("format") && m.contains("not supported"))
}

#[derive(Clone)]
struct ChunkLink {
    url: String,
    chunk_index: u64,
    /// When the presigned URL stops working (from the API's `expiration`).
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ChunkLink {
    /// Expired or about to: renew before use (links live ≤ 15 minutes, and a
    /// download started at the last second can still fail).
    fn expiring(&self) -> bool {
        self.expires_at.is_some_and(|t| t - chrono::Utc::now() < chrono::Duration::seconds(LINK_MARGIN_SECS))
    }
}

fn parse_link(l: &serde_json::Value) -> Option<ChunkLink> {
    Some(ChunkLink {
        url: l["external_link"].as_str()?.to_string(),
        chunk_index: l["chunk_index"].as_u64().unwrap_or(0),
        expires_at: l["expiration"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&chrono::Utc)),
    })
}

/// First external link of a result chunk (EXTERNAL_LINKS disposition).
fn first_link(chunk: &serde_json::Value) -> Option<ChunkLink> {
    parse_link(chunk.pointer("/external_links/0")?)
}

/// Attempts per chunk download (network errors, throttling, 5xx).
const DOWNLOAD_ATTEMPTS: u32 = 7;
/// Renewals of an expired presigned link per chunk.
const MAX_LINK_REFRESHES: u32 = 3;
/// Renew a link this many seconds before it expires.
const LINK_MARGIN_SECS: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Network error, timeout, 408/429/5xx: back off and retry.
    Transient,
    /// 400/403/404 from storage: the presigned link expired; renew it.
    Expired,
    Fatal,
}

struct FetchFail {
    class: Retry,
    retry_after: Option<std::time::Duration>,
    message: String,
}

/// Backoff before retry `attempt` (0-based): 250 ms × 2ⁿ capped at 8 s, with
/// ±50% jitter so parallel downloads don't retry in lockstep. Retry-After wins.
fn backoff_delay(attempt: u32, retry_after: Option<std::time::Duration>) -> std::time::Duration {
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

async fn backoff_sleep(attempt: u32, retry_after: Option<std::time::Duration>) {
    tokio::time::sleep(backoff_delay(attempt, retry_after)).await;
}

/// Server errors that mean EXTERNAL_LINKS is not available for this workspace.
fn mentions_links(msg: &str) -> bool {
    if is_inline_limit(msg) {
        return false;
    }
    let m = msg.to_ascii_lowercase();
    m.contains("external_links") || m.contains("external links") || m.contains("disposition")
}

/// "Inline byte limit exceeded" (INLINE results are capped at 25 MiB).
fn is_inline_limit(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("inline byte limit") || (m.contains("inline") && m.contains("limit") && m.contains("external"))
}

/// Drop query strings from URLs in error text (presigned URLs carry credentials).
fn redact_url(s: &str) -> String {
    s.split_whitespace()
        .map(|w| match w.find('?') {
            Some(i) if w.contains("://") => format!("{}?…", &w[..i]),
            _ => w.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Result columns from the statement manifest.
fn manifest_columns(resp: &serde_json::Value) -> Vec<Column> {
    resp.pointer("/manifest/schema/columns")
        .and_then(|c| c.as_array())
        .map(|cols| {
            cols.iter()
                .map(|c| {
                    let t = c["type_name"].as_str().unwrap_or("STRING");
                    let text = c["type_text"].as_str().unwrap_or(t);
                    Column::new(c["name"].as_str().unwrap_or(""), col_type(t), text.to_ascii_lowercase())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Decode an Arrow IPC stream chunk and conform its columns to `schema` (the
/// same types the JSON path produces: exact decimals and complex values as
/// text, timestamps in microseconds), so results look the same either way.
fn decode_arrow(bytes: &[u8], schema: &SchemaRef) -> Result<Vec<RecordBatch>> {
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
                let direct = if col.data_type().is_nested() { None } else { cast_with_options(col, f.data_type(), &CastOptions { safe: true, ..Default::default() }).ok() };
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

fn split_schema(path: &str) -> Result<(&str, &str)> {
    path.split_once('.').ok_or_else(|| ConnectorError::query(format!("expected catalog.schema, got {path}")))
}

fn cell(r: &[serde_json::Value], i: usize) -> Option<String> {
    r.get(i).and_then(|v| v.as_str()).map(str::to_string)
}

#[async_trait]
impl Session for DbxSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Databricks
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.0.run_small("SELECT current_version().dbsql_version").await.unwrap_or_default();
        let v = r.first().and_then(|r| cell(r, 0)).unwrap_or_default();
        Ok(format!("Databricks SQL {v} (warehouse {})", self.0.warehouse).replace("  ", " "))
    }

    async fn ping(&self) -> Result<()> {
        self.0.run_small("SELECT 1").await.map(|_| ())
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let inner = self.0.clone();
        let sql = sql.to_string();
        tokio::spawn(async move {
            if let Err(e) = inner.stream(&sql, &opts, &tx).await {
                tx.send_err(e).await;
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let default_cat = self.0.catalog.lock().await.clone();
        let default_sch = self.0.schema.lock().await.clone().unwrap_or_else(|| "default".into());
        let rows = self
            .0
            .run_small(
                "SELECT catalog_name, schema_name FROM system.information_schema.schemata \
                 WHERE schema_name <> 'information_schema' ORDER BY catalog_name, schema_name",
            )
            .await;
        let rows = match rows {
            Ok(r) => r,
            Err(_) => {
                // Workspaces without Unity Catalog: hive_metastore only.
                let r = self.0.run_small("SHOW SCHEMAS IN hive_metastore").await?;
                r.into_iter().map(|x| vec![json!("hive_metastore"), x.first().cloned().unwrap_or_default()]).collect()
            }
        };
        let mut out: Vec<SchemaInfo> = rows
            .iter()
            .filter_map(|r| {
                let (c, s) = (cell(r, 0)?, cell(r, 1)?);
                let is_default = default_cat.as_deref().is_none_or(|d| d == c) && s == default_sch;
                Some(SchemaInfo::in_catalog(c, &s, is_default))
            })
            .collect();
        out.sort_by_key(|s| !s.is_default);
        Ok(out)
    }

    async fn search_objects(&self, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let term = databrain_connector_core::search_sql_term(query);
        let sql = format!(
            "SELECT table_catalog, table_schema, table_name, table_type, comment FROM system.information_schema.tables \
             WHERE table_schema <> 'information_schema' AND instr(lower(table_name), {}) > 0 LIMIT {}",
            quote_literal(&term),
            (limit * 4).max(200)
        );
        let rows = match self.0.run_small(&sql).await {
            Ok(r) => r,
            // No Unity Catalog: walk hive_metastore schema by schema.
            Err(_) => return databrain_connector_core::search_by_listing(self, query, limit).await,
        };
        let mut hits: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                let t = cell(r, 3).unwrap_or_default();
                Some(DbObject {
                    schema: format!("{}.{}", cell(r, 0)?, cell(r, 1)?),
                    name: cell(r, 2)?,
                    kind: if t.contains("VIEW") { if t.contains("MATERIALIZED") { ObjectKind::MaterializedView } else { ObjectKind::View } } else { ObjectKind::Table },
                    comment: cell(r, 4),
                    row_estimate: None,
                })
            })
            .filter(|o| databrain_connector_core::object_matches(query, &o.schema, &o.name))
            .collect();
        databrain_connector_core::rank_matches(query, &mut hits, limit);
        Ok(hits)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let (cat, sch) = split_schema(schema)?;
        let sql = format!(
            "SELECT table_name, table_type, comment FROM {}.information_schema.tables WHERE table_schema = {} ORDER BY table_type, table_name",
            quote_ident(ConnectorKind::Databricks, cat),
            quote_literal(sch)
        );
        let rows = match self.0.run_small(&sql).await {
            Ok(r) => r,
            Err(_) => {
                let r = self.0.run_small(&format!("SHOW TABLES IN {}", databrain_connector_core::quote_path(ConnectorKind::Databricks, schema))).await?;
                r.into_iter().map(|x| vec![x.get(1).cloned().unwrap_or_default(), json!("TABLE"), serde_json::Value::Null]).collect()
            }
        };
        let mut out: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                let name = cell(r, 0)?;
                let t = cell(r, 1).unwrap_or_default();
                Some(DbObject {
                    schema: schema.to_string(),
                    name,
                    kind: if t.contains("VIEW") { if t.contains("MATERIALIZED") { ObjectKind::MaterializedView } else { ObjectKind::View } } else { ObjectKind::Table },
                    comment: cell(r, 2),
                    row_estimate: None,
                })
            })
            .collect();
        // Unity Catalog functions (best effort).
        let q = format!(
            "SELECT routine_name, comment FROM {}.information_schema.routines WHERE routine_schema = {} ORDER BY routine_name",
            quote_ident(ConnectorKind::Databricks, cat),
            quote_literal(sch)
        );
        if let Ok(rows) = self.0.run_small(&q).await {
            out.extend(rows.iter().filter_map(|r| Some(DbObject { schema: schema.to_string(), name: cell(r, 0)?, kind: ObjectKind::Function, comment: cell(r, 1), row_estimate: None })));
        }
        Ok(out)
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        use databrain_connector_core::{parse_list_text, query_table, TableLayout};
        let full = format!("{}.{}", databrain_connector_core::quote_path(ConnectorKind::Databricks, schema), quote_ident(ConnectorKind::Databricks, name));
        let mut l = TableLayout::default();
        // Views and non-Delta tables have no detail: no hints, not an error.
        let Ok((names, rows)) = query_table(self, &format!("DESCRIBE DETAIL {full}")).await else { return Ok(l) };
        let Some(r) = rows.first() else { return Ok(l) };
        let get = |n: &str| names.iter().position(|x| x.eq_ignore_ascii_case(n)).and_then(|i| r.get(i).cloned().flatten());
        l.partition_by = get("partitionColumns").as_deref().map(parse_list_text).unwrap_or_default();
        if !l.partition_by.is_empty() {
            l.partition_kind = Some("identity".into());
        }
        l.cluster_by = get("clusteringColumns").as_deref().map(parse_list_text).unwrap_or_default();
        if let (Some(files), Some(bytes)) = (get("numFiles"), get("sizeInBytes").and_then(|b| b.parse::<f64>().ok())) {
            l.notes.push(format!("{} · {files} files · {:.1} GB", get("format").unwrap_or_else(|| "delta".into()), bytes / 1e9));
        }
        Ok(l)
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let object = self
            .list_objects(schema)
            .await?
            .into_iter()
            .filter(|o| o.name == name)
            .min_by_key(|o| !o.kind.is_relation())
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let cols = self.0.schema_columns_filtered(schema, Some(name)).await?;
        let columns = cols.into_iter().next().map(|t| t.columns).unwrap_or_default();
        let full = format!("{}.{}", databrain_connector_core::quote_path(ConnectorKind::Databricks, schema), quote_ident(ConnectorKind::Databricks, name));
        let ddl = self
            .0
            .run_small(&format!("SHOW CREATE TABLE {full}"))
            .await
            .ok()
            .and_then(|r| r.first().and_then(|r| cell(r, 0)));
        Ok(ObjectDetail { object, columns, ddl, foreign_keys: vec![] })
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        self.0.schema_columns_filtered(schema, None).await
    }

    /// Two queries per catalog (tables + columns from the catalog's
    /// information schema) instead of two per schema.
    async fn bulk_metadata(&self, schemas: &[String]) -> Result<Vec<SchemaMetadata>> {
        let mut by_cat: Vec<(String, Vec<String>)> = Vec::new();
        for s in schemas {
            let (c, sch) = split_schema(s)?;
            match by_cat.iter_mut().find(|(k, _)| k == c) {
                Some((_, v)) => v.push(sch.to_string()),
                None => by_cat.push((c.to_string(), vec![sch.to_string()])),
            }
        }
        let mut out = Vec::with_capacity(schemas.len());
        for (cat, list) in by_cat {
            for chunk in list.chunks(200) {
                match self.0.catalog_metadata(&cat, chunk).await {
                    Ok(v) => out.extend(v),
                    // e.g. hive_metastore without information_schema: per schema.
                    Err(_) => {
                        let ids: Vec<String> = chunk.iter().map(|s| format!("{cat}.{s}")).collect();
                        out.extend(databrain_connector_core::default_bulk_metadata(self, &ids).await?);
                    }
                }
            }
        }
        Ok(out)
    }

    async fn schema_object_counts(&self) -> Result<Option<std::collections::HashMap<String, usize>>> {
        let rows = match self
            .0
            .run_small("SELECT table_catalog, table_schema, count(*) FROM system.information_schema.tables GROUP BY 1, 2")
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        Ok(Some(
            rows.iter()
                .filter_map(|r| Some((format!("{}.{}", cell(r, 0)?, cell(r, 1)?), cell(r, 2)?.parse().ok()?)))
                .collect(),
        ))
    }
}

impl Inner {
    async fn catalog_metadata(&self, cat: &str, schemas: &[String]) -> Result<Vec<SchemaMetadata>> {
        let list = schemas.iter().map(|s| quote_literal(s)).collect::<Vec<_>>().join(", ");
        let cq = quote_ident(ConnectorKind::Databricks, cat);
        let tables = self
            .run_small(&format!(
                "SELECT table_schema, table_name, table_type, comment FROM {cq}.information_schema.tables WHERE table_schema IN ({list})"
            ))
            .await?;
        let cols = self
            .run_small(&format!(
                "SELECT table_schema, table_name, column_name, full_data_type, is_nullable, comment FROM {cq}.information_schema.columns \
                 WHERE table_schema IN ({list}) ORDER BY table_schema, table_name, ordinal_position"
            ))
            .await?;
        let mut out: Vec<SchemaMetadata> = schemas
            .iter()
            .map(|s| SchemaMetadata { schema: format!("{cat}.{s}"), objects: vec![], columns: vec![], error: None })
            .collect();
        let idx = |s: &str| schemas.iter().position(|x| x == s);
        for r in &tables {
            let (Some(s), Some(name)) = (cell(r, 0), cell(r, 1)) else { continue };
            let Some(i) = idx(&s) else { continue };
            let t = cell(r, 2).unwrap_or_default();
            let schema = out[i].schema.clone();
            out[i].objects.push(DbObject {
                schema,
                name,
                kind: if t.contains("VIEW") { if t.contains("MATERIALIZED") { ObjectKind::MaterializedView } else { ObjectKind::View } } else { ObjectKind::Table },
                comment: cell(r, 3),
                row_estimate: None,
            });
        }
        for r in &cols {
            let (Some(s), Some(table)) = (cell(r, 0), cell(r, 1)) else { continue };
            let Some(i) = idx(&s) else { continue };
            let col = ColumnInfo {
                name: cell(r, 2).unwrap_or_default(),
                data_type: cell(r, 3).unwrap_or_default(),
                nullable: cell(r, 4).as_deref() != Some("NO"),
                is_primary_key: false,
                default: None,
                comment: cell(r, 5),
            };
            let cs = &mut out[i].columns;
            match cs.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => cs.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        Ok(out)
    }

    async fn schema_columns_filtered(&self, schema: &str, table: Option<&str>) -> Result<Vec<TableColumns>> {
        let (cat, sch) = split_schema(schema)?;
        let mut sql = format!(
            "SELECT table_name, column_name, full_data_type, is_nullable, comment FROM {}.information_schema.columns WHERE table_schema = {}",
            quote_ident(ConnectorKind::Databricks, cat),
            quote_literal(sch)
        );
        if let Some(t) = table {
            sql.push_str(&format!(" AND table_name = {}", quote_literal(t)));
        }
        sql.push_str(" ORDER BY table_name, ordinal_position");
        let rows = self.run_small(&sql).await?;
        let mut out: Vec<TableColumns> = Vec::new();
        for r in rows {
            let table = cell(&r, 0).unwrap_or_default();
            let col = ColumnInfo {
                name: cell(&r, 1).unwrap_or_default(),
                data_type: cell(&r, 2).unwrap_or_default(),
                nullable: cell(&r, 3).as_deref() != Some("NO"),
                is_primary_key: false,
                default: None,
                comment: cell(&r, 4),
            };
            match out.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => out.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_warehouse_id() {
        let mut c = ConnectionConfig::new(ConnectorKind::Databricks, AuthMethod::ApiToken { user: None });
        c.options.insert("http_path".into(), "/sql/1.0/warehouses/abc123/".into());
        assert_eq!(warehouse_id(&c).unwrap(), "abc123");
    }

    #[test]
    fn oauth_endpoints() {
        let mut c = ConnectionConfig::new(ConnectorKind::Databricks, AuthMethod::OauthBrowser(Default::default()));
        c.host = Some("https://adb-1.azuredatabricks.net/".into());
        let ctx = DatabricksConnector.auth_context(&c);
        let o = ctx.oauth.unwrap();
        assert_eq!(o.authorize_url, "https://adb-1.azuredatabricks.net/oidc/v1/authorize");
        assert_eq!(o.client_id, "databricks-cli");
        assert_eq!(o.redirect_port, Some(8020));
        assert_eq!(o.scopes, vec!["sql", "offline_access"]);
    }

    #[test]
    fn maps_types() {
        assert_eq!(col_type("BIGINT"), ColType::Int64);
        assert_eq!(col_type("DECIMAL"), ColType::Utf8);
        assert_eq!(col_type("TIMESTAMP"), ColType::TimestampTz);
    }

    /// Live: DATABRAIN_DBX_HOST, _HTTP_PATH, _TOKEN.
    #[tokio::test]
    async fn live_roundtrip() {
        let (Ok(host), Ok(path), Ok(token)) = (
            std::env::var("DATABRAIN_DBX_HOST"),
            std::env::var("DATABRAIN_DBX_HTTP_PATH"),
            std::env::var("DATABRAIN_DBX_TOKEN"),
        ) else {
            eprintln!("skipping: DATABRAIN_DBX_* not set");
            return;
        };
        use databrain_auth::InlineCredentialSource;
        let mut c = ConnectionConfig::new(ConnectorKind::Databricks, AuthMethod::ApiToken { user: None });
        c.host = Some(host);
        c.options.insert("http_path".into(), path);
        let creds = Arc::new(InlineCredentialSource::new(AuthMethod::ApiToken { user: None }, Some(token.into())));
        let s = DatabricksConnector.connect(&c, creds).await.unwrap();
        let r = s.execute("SELECT 1 AS a, 'x' AS b, current_timestamp() AS c", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), 1);
        assert!(!s.list_schemas().await.unwrap().is_empty());
    }

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

    /// Fake Databricks + cloud storage on localhost.
    mod mock {
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
                                let len = head.lines().find_map(|h| h.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
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
                            let (sig, exp) = if fresh { ("secret", chrono::Utc::now() + chrono::Duration::minutes(10)) } else { ("old", chrono::Utc::now() - chrono::Duration::minutes(1)) };
                            let mut e = json!({"chunk_index": i, "external_link": format!("{o}/blob/{fmt}/{i}?sig={sig}"), "row_count": 2, "expiration": exp.to_rfc3339()});
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
                                (200, json!({"statement_id": "st2", "status": {"state": "SUCCEEDED"}, "manifest": {"schema": cols}, "result": {"data_array": [["7", "inline"]]}}))
                            } else if v["format"] == "ARROW_STREAM" && blob_mode == "no_arrow" {
                                (400, json!({"message": "ARROW_STREAM format is not supported for this warehouse"}))
                            } else if v["format"] == "ARROW_STREAM" {
                                *lg.lock().unwrap().arrow_posts.get_or_insert(0) += 1;
                                (200, json!({"statement_id": "st1", "status": {"state": "SUCCEEDED"}, "manifest": {"schema": cols, "total_chunk_count": CHUNKS}, "result": first("arrow", false)}))
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
                        let head = format!("HTTP/1.1 {status} X\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
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

    fn texts(r: &databrain_connector_core::Collected) -> Vec<String> {
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

    fn ints(r: &databrain_connector_core::Collected) -> Vec<i64> {
        use databrain_connector_core::arrow::array::AsArray;
        use databrain_connector_core::arrow::datatypes::Int64Type;
        r.batches.iter().flat_map(|b| b.column(0).as_primitive::<Int64Type>().values().to_vec()).collect()
    }

    #[tokio::test]
    async fn arrow_chunks_download_in_parallel_and_in_order() {
        let (origin, log) = mock::serve("ok").await;
        let s = mock::session(&origin, false);
        let opts = ExecOptions { max_rows: Some(1001), ..Default::default() };
        let t0 = std::time::Instant::now();
        let r = s.execute("SELECT * FROM big", opts).await.unwrap().collect().await.unwrap();
        let took = t0.elapsed();
        let n = mock::CHUNKS as i64;
        assert_eq!(ints(&r), (0..n * 2).collect::<Vec<_>>(), "rows in chunk order, n cast to BIGINT");
        assert_eq!(texts(&r)[..4], ["r0", "NULL", "r2", "NULL"]);
        assert_eq!(r.schema.as_ref().unwrap().field(0).data_type(), &databrain_connector_core::arrow::datatypes::DataType::Int64);
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
        let r = mock::session(&origin, false).execute("SELECT * FROM big", ExecOptions::default()).await.unwrap().collect().await.unwrap();
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
        let r = s.execute("SELECT * FROM big", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), mock::CHUNKS as usize * 2);
        assert!(log.lock().unwrap().requests.iter().any(|(_, p, _, _)| p == "/api/2.0/sql/statements/st1/result/chunks/0"), "fresh link requested");
    }

    #[tokio::test]
    async fn blocked_storage_falls_back_to_inline() {
        let (origin, _) = mock::serve("blocked").await;
        let s = mock::session(&origin, false);
        let r = s.execute("SELECT * FROM big", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(texts(&r), vec!["inline"]);
        assert!(r.notices.iter().any(|n| n.contains("inline")), "{:?}", r.notices);
        // Inline-only connections never try links.
        let (origin, log) = mock::serve("ok").await;
        let r = mock::session(&origin, true).execute("SELECT 1", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(texts(&r), vec!["inline"]);
        assert!(log.lock().unwrap().requests.iter().all(|(_, p, _, _)| !p.starts_with("/blob/")));
    }

    #[tokio::test]
    async fn dropped_connections_and_503_are_retried() {
        let (origin, log) = mock::serve("flaky").await;
        let r = mock::session(&origin, false).execute("SELECT * FROM big", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(ints(&r), (0..mock::CHUNKS as i64 * 2).collect::<Vec<_>>());
        let log = log.lock().unwrap();
        assert!(log.hits.values().all(|h| *h == 3), "drop, 503, then ok for every chunk: {:?}", log.hits);
    }

    #[tokio::test]
    async fn expired_links_are_renewed_before_download() {
        let (origin, log) = mock::serve("stale_links").await;
        let r = mock::session(&origin, false).execute("SELECT * FROM big", ExecOptions::default()).await.unwrap().collect().await.unwrap();
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
