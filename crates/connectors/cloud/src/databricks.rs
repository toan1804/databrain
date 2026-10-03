//! Databricks SQL via the Statement Execution API 2.0.
//!
//! Auth: personal access token, OAuth U2M (browser + PKCE against the
//! workspace's `/oidc` endpoints with the `databricks-cli` public client),
//! OAuth M2M (service principal) and existing Databricks CLI profiles.
//! Names are three-level; schemas are exposed as `catalog.schema`.

use std::sync::Arc;

use async_trait::async_trait;
use databrain_auth::{AuthContext, AuthMethod, AuthMethodKind, CliKind, ClientAuthStyle, Credential, CredentialSource, OAuthConfig};
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
            "format": "JSON_ARRAY",
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
        // up to 100 GiB). Workspaces that block those links fall back to INLINE.
        let first = if self.inline_only { Disposition::Inline } else { Disposition::ExternalLinks };
        match self.stream_as(sql, opts, tx, first).await {
            Err(StreamFail::Links(e)) if first == Disposition::ExternalLinks => {
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

    async fn stream_as(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender, disposition: Disposition) -> std::result::Result<(), StreamFail> {
        // Emulate USE for subsequent statements (the API is stateless).
        let kw = databrain_connector_core::sql::leading_keyword(sql);
        let resp = match self.submit_as(sql, opts.max_rows, disposition).await {
            Ok(r) => r,
            // Links disabled for this workspace: rejected at submit time.
            Err(e) if disposition == Disposition::ExternalLinks && mentions_links(&e.message) => return Err(StreamFail::Links(e)),
            Err(e) => return Err(StreamFail::Other(e)),
        };
        let resp = match self.wait(resp, &opts.cancel).await {
            Ok(r) => r,
            Err(e) if disposition == Disposition::ExternalLinks && mentions_links(&e.message) => return Err(StreamFail::Links(e)),
            Err(e) => return Err(StreamFail::Other(e)),
        };
        if kw == "USE" {
            self.apply_use(sql).await;
        }
        let statement_id = resp["statement_id"].as_str().unwrap_or_default().to_string();
        let columns: Vec<Column> = resp
            .pointer("/manifest/schema/columns")
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
            .unwrap_or_default();
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
        match self.fetch_chunk(&link.url).await {
            Ok(rows) => Ok(rows),
            // Expired (links live ≤ 15 minutes): ask for a fresh one, once.
            Err((Some(400 | 403 | 404), _)) if !statement_id.is_empty() => {
                let fresh = self
                    .request(reqwest::Method::GET, format!("{}/{statement_id}/result/chunks/{}", self.base, link.chunk_index), None)
                    .await?;
                let link = first_link(&fresh).ok_or_else(|| ConnectorError::internal("result chunk has no download link"))?;
                self.fetch_chunk(&link.url).await.map_err(|(_, e)| e)
            }
            Err((_, e)) => Err(e),
        }
    }

    /// GET a presigned URL; the error carries the HTTP status when there was one.
    async fn fetch_chunk(&self, url: &str) -> std::result::Result<Vec<serde_json::Value>, (Option<u16>, ConnectorError)> {
        let fail = |e: reqwest::Error| (None, ConnectorError::connection(format!("result download failed: {}", redact_url(&e.to_string()))));
        let resp = self.http.get(url).send().await.map_err(fail)?;
        let status = resp.status();
        if !status.is_success() {
            return Err((Some(status.as_u16()), ConnectorError::connection(format!("result download failed: HTTP {status}"))));
        }
        let bytes = resp.bytes().await.map_err(fail)?;
        serde_json::from_slice(&bytes).map_err(|e| (None, ConnectorError::internal(format!("unexpected result chunk: {e}"))))
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
    ExternalLinks,
}

impl Disposition {
    fn as_str(self) -> &'static str {
        match self {
            Disposition::Inline => "INLINE",
            Disposition::ExternalLinks => "EXTERNAL_LINKS",
        }
    }
}

/// Why a streamed statement failed: external links could not be used
/// (retry inline), or anything else.
enum StreamFail {
    Links(ConnectorError),
    Other(ConnectorError),
}

impl StreamFail {
    fn into_inner(self) -> ConnectorError {
        match self {
            StreamFail::Links(e) | StreamFail::Other(e) => e,
        }
    }
}

struct ChunkLink {
    url: String,
    chunk_index: u64,
}

/// First external link of a result chunk (EXTERNAL_LINKS disposition).
fn first_link(chunk: &serde_json::Value) -> Option<ChunkLink> {
    let l = chunk.pointer("/external_links/0")?;
    Some(ChunkLink { url: l["external_link"].as_str()?.to_string(), chunk_index: l["chunk_index"].as_u64().unwrap_or(0) })
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

        #[derive(Default)]
        pub struct Log {
            pub requests: Vec<(String, String, bool, String)>, // method, path, had auth header, body
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
                        let link = |i: u32, next: bool| {
                            let mut e = json!({"chunk_index": i, "external_link": format!("{o}/blob/{i}?sig=secret"), "row_count": 2});
                            if next {
                                e["next_chunk_internal_link"] = json!(format!("/api/2.0/sql/statements/st1/result/chunks/{}", i + 1));
                            }
                            json!({"external_links": [e]})
                        };
                        let (status, out) = if method == "POST" && path == "/api/2.0/sql/statements" {
                            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                            if v["disposition"] == "INLINE" {
                                (200, json!({"statement_id": "st2", "status": {"state": "SUCCEEDED"}, "manifest": {"schema": cols}, "result": {"data_array": [["7", "inline"]]}}))
                            } else {
                                (200, json!({"statement_id": "st1", "status": {"state": "SUCCEEDED"}, "manifest": {"schema": cols}, "result": link(0, true)}))
                            }
                        } else if path == "/api/2.0/sql/statements/st1/result/chunks/1" {
                            (200, link(1, false))
                        } else if path == "/api/2.0/sql/statements/st1/result/chunks/0" {
                            (200, link(0, true))
                        } else if let Some(rest) = path.strip_prefix("/blob/") {
                            let i: u32 = rest.split('?').next().unwrap().parse().unwrap();
                            let deny = blob_mode == "blocked" || (blob_mode == "expire_once" && !expired.swap(true, std::sync::atomic::Ordering::SeqCst));
                            if deny {
                                (403, json!({"error": "expired"}))
                            } else {
                                (200, json!([[format!("{}", i * 2), format!("r{}", i * 2)], [format!("{}", i * 2 + 1), null]]))
                            }
                        } else {
                            (404, json!({"message": "not found"}))
                        };
                        let text = out.to_string();
                        let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
                        let _ = sock.write_all(resp.as_bytes()).await;
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

    #[tokio::test]
    async fn large_results_use_external_links() {
        let (origin, log) = mock::serve("ok").await;
        let s = mock::session(&origin, false);
        let opts = ExecOptions { max_rows: Some(1001), ..Default::default() };
        let r = s.execute("SELECT * FROM big", opts).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), 4, "two chunks of two rows");
        assert_eq!(texts(&r), vec!["r0", "NULL", "r2", "NULL"]);
        let log = log.lock().unwrap();
        let post: serde_json::Value = serde_json::from_str(&log.requests[0].3).unwrap();
        assert_eq!(post["disposition"], "EXTERNAL_LINKS");
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
    async fn expired_link_is_renewed() {
        let (origin, log) = mock::serve("expire_once").await;
        let s = mock::session(&origin, false);
        let r = s.execute("SELECT * FROM big", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), 4);
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
}
