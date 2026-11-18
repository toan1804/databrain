//! Google BigQuery via the REST Jobs API.
//!
//! Auth: Google sign-in in the browser (installed-app OAuth + PKCE, needs an
//! OAuth client ID of type "Desktop app"), service account JSON keys, and
//! gcloud Application Default Credentials. Statements run inside a BigQuery
//! session so temp tables and variables persist across statements in a tab.
//! Schemas are exposed as `project.dataset`.

use std::sync::Arc;

use async_trait::async_trait;
use databrain_auth::{AuthContext, AuthMethodKind, CliKind, ClientAuthStyle, Credential, CredentialSource, OAuthConfig};
use databrain_connector_core::value::Column;
use databrain_connector_core::{
    Capabilities, ColType, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, CostEstimate, DbObject, ExecOptions,
    ExecSummary, FieldSpec, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session, StreamEvent, StreamSender, TableColumns, Value,
};
use secrecy::ExposeSecret;
use serde_json::json;
use tokio::sync::Mutex;

use crate::common::{Backoff, RowSink, http, json_body, net_err, text_cell};

const API: &str = "https://bigquery.googleapis.com/bigquery/v2";
pub const SCOPES: &[&str] = &["https://www.googleapis.com/auth/bigquery", "https://www.googleapis.com/auth/cloud-platform.read-only"];

#[derive(Debug, Default)]
pub struct BigQueryConnector;

impl BigQueryConnector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Connector for BigQueryConnector {
    fn validate(&self, cfg: &ConnectionConfig) -> std::result::Result<(), String> {
        if cfg.opt("project").is_none() {
            return Err("Billing project ID is required".into());
        }
        Ok(())
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Bigquery,
            traits: ConnectorKind::Bigquery.traits(),
            display_name: "BigQuery",
            default_port: None,
            uses_file: false,
            auth_methods: vec![AuthMethodKind::OauthBrowser, AuthMethodKind::ServiceAccount, AuthMethodKind::CloudCli],
            capabilities: Capabilities { ssh: false },
            fields: vec![
                FieldSpec::new("project", "Billing project ID").required().placeholder("my-gcp-project"),
                FieldSpec::new("location", "Location").placeholder("US / EU / asia-southeast1"),
                FieldSpec::new("dataset", "Default dataset").placeholder("(optional)"),
                FieldSpec::new("maximum_bytes_billed", "Max bytes billed per query").placeholder("(optional) e.g. 10000000000"),
                FieldSpec::new(LOAD_JOBS_OPTION, "Load jobs for loads (beta)").placeholder("off").help(
                    "on: job loads and \"Load into…\" upload the rows and make one load job (all rows or none, no query cost) instead of INSERT jobs. Each job counts toward the table's daily modification limit. Off by default until it has been tried against live projects.",
                ),
            ],
            note: Some("Browser sign-in needs a Google OAuth client ID (Desktop app). Service accounts need BigQuery Job User."),
        }
    }

    fn auth_context(&self, cfg: &ConnectionConfig) -> AuthContext {
        let p = cfg.auth.oauth_params().cloned().unwrap_or_default();
        let scopes: Vec<String> =
            p.scopes.as_deref().map(|s| s.split_whitespace().map(str::to_string).collect()).unwrap_or_else(|| SCOPES.iter().map(|s| s.to_string()).collect());
        let mut extra = vec![("access_type".to_string(), "offline".to_string()), ("prompt".to_string(), "consent".to_string())];
        if let Some(u) = p.user.clone() {
            extra.push(("login_hint".into(), u));
        }
        AuthContext {
            oauth: Some(OAuthConfig {
                provider: "Google".into(),
                authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                token_url: databrain_auth::cloud::GOOGLE_TOKEN_URL.into(),
                device_url: Some("https://oauth2.googleapis.com/device/code".into()),
                revoke_url: Some("https://oauth2.googleapis.com/revoke".into()),
                client_id: p.client_id.unwrap_or_default(),
                client_secret: None,
                scopes: scopes.clone(),
                redirect_host: "127.0.0.1".into(),
                redirect_port: p.redirect_port,
                redirect_path: "/".into(),
                extra_authorize_params: extra,
                client_auth: ClientAuthStyle::Body,
            }),
            cli: Some(CliKind::GoogleAdc { scopes: scopes.clone() }),
            google_scopes: scopes,
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let project = cfg
            .opt("project")
            .or(cfg.database.as_deref().filter(|d| !d.is_empty()))
            .ok_or_else(|| ConnectorError::config("BigQuery needs a billing project ID"))?
            .to_string();
        let s = BqSession(Arc::new(Inner {
            project,
            location: cfg.opt("location").map(str::to_string),
            dataset: cfg.opt("dataset").map(str::to_string),
            max_bytes: cfg.opt("maximum_bytes_billed").map(str::to_string),
            creds,
            http: http(),
            session_id: Mutex::new(None),
            api: API.to_string(),
            load_jobs: cfg.opt(LOAD_JOBS_OPTION).is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "true" | "yes" | "1")),
        }));
        // Validate access.
        s.0.call(reqwest::Method::GET, format!("{API}/projects/{}/datasets?maxResults=1", s.0.project), None).await?;
        Ok(Box::new(s))
    }
}

pub struct BqSession(Arc<Inner>);

struct Inner {
    project: String,
    location: Option<String>,
    dataset: Option<String>,
    max_bytes: Option<String>,
    creds: Arc<dyn CredentialSource>,
    http: reqwest::Client,
    session_id: Mutex<Option<String>>,
    /// The API's base URL for query jobs ([`API`]; a local server in tests).
    api: String,
    /// Loads go by load jobs ([`LOAD_JOBS_OPTION`]).
    load_jobs: bool,
}

/// A query job: its id and where it runs. `jobs.query` answers with the
/// job's own location (`jobReference.location`), which is where it is
/// polled and cancelled; the connection's location is only a default.
struct JobRef {
    id: String,
    location: Option<String>,
}

impl JobRef {
    fn of(resp: &serde_json::Value, default_location: Option<&str>) -> Self {
        let r = &resp["jobReference"];
        JobRef {
            id: r["jobId"].as_str().unwrap_or_default().to_string(),
            location: r["location"].as_str().map(str::to_string).or(default_location.map(str::to_string)),
        }
    }

    /// `location=…` for a request's query string (empty when unknown).
    fn loc_param(&self) -> String {
        self.location.as_deref().map(|l| format!("location={}", urlencode(l))).unwrap_or_default()
    }
}

fn col_type(t: &str) -> ColType {
    match t {
        "BOOLEAN" | "BOOL" => ColType::Bool,
        "INTEGER" | "INT64" => ColType::Int64,
        "FLOAT" | "FLOAT64" => ColType::Float64,
        "DATE" => ColType::Date,
        "TIME" => ColType::Time,
        "DATETIME" => ColType::Timestamp,
        "TIMESTAMP" => ColType::TimestampTz,
        "BYTES" => ColType::Utf8, // base64 text
        _ => ColType::Utf8,       // NUMERIC/BIGNUMERIC exact, STRING, JSON, GEOGRAPHY, RECORD
    }
}

/// Convert a BigQuery `f/v` cell. TIMESTAMP arrives as int64 microseconds
/// (useInt64Timestamp); nested RECORD/REPEATED values become JSON text.
fn bq_cell(t: ColType, field: &serde_json::Value, v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    let repeated = field["mode"].as_str() == Some("REPEATED");
    let record = matches!(field["type"].as_str(), Some("RECORD") | Some("STRUCT"));
    if repeated || record {
        return match v {
            J::Null => Value::Null,
            other => Value::Text(plain_json(field, other).to_string()),
        };
    }
    match v {
        J::Null => Value::Null,
        J::String(s) if t == ColType::TimestampTz => s
            .parse::<i64>()
            .map(Value::TimestampTz)
            .or_else(|_| s.parse::<f64>().map(|f| Value::TimestampTz((f * 1e6) as i64)))
            .unwrap_or_else(|_| Value::Text(s.clone())),
        J::String(s) => text_cell(t, s),
        other => Value::Text(other.to_string()),
    }
}

/// Turn BigQuery's `{"f":[{"v":..}]}` / `[{"v":..}]` encoding into plain JSON.
fn plain_json(field: &serde_json::Value, v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value as J;
    let repeated = field["mode"].as_str() == Some("REPEATED");
    if repeated {
        let elem_field = {
            let mut f = field.clone();
            f["mode"] = json!("NULLABLE");
            f
        };
        return J::Array(v.as_array().map(|a| a.iter().map(|x| plain_json(&elem_field, x.get("v").unwrap_or(x))).collect()).unwrap_or_default());
    }
    if matches!(field["type"].as_str(), Some("RECORD") | Some("STRUCT")) {
        let mut obj = serde_json::Map::new();
        let subs = field["fields"].as_array().cloned().unwrap_or_default();
        let vals = v["f"].as_array().cloned().unwrap_or_default();
        for (sf, sv) in subs.iter().zip(vals.iter()) {
            obj.insert(sf["name"].as_str().unwrap_or("").to_string(), plain_json(sf, &sv["v"]));
        }
        return J::Object(obj);
    }
    match (field["type"].as_str(), v) {
        (Some("INTEGER") | Some("INT64"), J::String(s)) => s.parse::<i64>().map(J::from).unwrap_or(v.clone()),
        (Some("FLOAT") | Some("FLOAT64"), J::String(s)) => s.parse::<f64>().map(J::from).unwrap_or(v.clone()),
        (Some("BOOLEAN") | Some("BOOL"), J::String(s)) => J::Bool(s == "true"),
        _ => v.clone(),
    }
}

impl Inner {
    async fn token(&self) -> Result<String> {
        match self.creds.get().await? {
            Credential::Bearer { token, .. } => Ok(token.expose_secret().to_string()),
            _ => Err(ConnectorError::config("BigQuery requires Google sign-in, a service account or gcloud credentials")),
        }
    }

    async fn call(&self, method: reqwest::Method, url: String, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
        for attempt in 0..2 {
            let mut req = self.http.request(method.clone(), &url).bearer_auth(self.token().await?);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(net_err)?;
            if resp.status().as_u16() == 401 && attempt == 0 {
                self.creds.invalidate().await;
                continue;
            }
            return json_body(resp, "BigQuery").await;
        }
        unreachable!()
    }

    /// Start a query job. `in_session` attaches it to this tab's session.
    async fn start(&self, sql: &str, in_session: bool, max_results: u32) -> Result<serde_json::Value> {
        let mut body = json!({
            "query": sql,
            "useLegacySql": false,
            "timeoutMs": 10000,
            "maxResults": max_results,
            "formatOptions": { "useInt64Timestamp": true },
        });
        if let Some(l) = &self.location {
            body["location"] = l.clone().into();
        }
        if let Some(d) = &self.dataset {
            body["defaultDataset"] = json!({ "projectId": self.project, "datasetId": d });
        }
        if let Some(m) = &self.max_bytes {
            body["maximumBytesBilled"] = m.clone().into();
        }
        if in_session {
            let sid = self.session_id.lock().await.clone();
            match sid {
                Some(id) => body["connectionProperties"] = json!([{ "key": "session_id", "value": id }]),
                None => body["createSession"] = json!(true),
            }
        }
        let resp = self.call(reqwest::Method::POST, format!("{}/projects/{}/queries", self.api, self.project), Some(body)).await?;
        if in_session && let Some(id) = resp.pointer("/sessionInfo/sessionId").and_then(|s| s.as_str()) {
            *self.session_id.lock().await = Some(id.to_string());
        }
        Ok(resp)
    }

    /// Dry run (`dryRun: true`): BigQuery plans the query and reports the
    /// bytes it would process; nothing runs and nothing is billed. On this
    /// tab's BigQuery session when it has one, so its temp tables resolve.
    async fn dry_run(&self, sql: &str) -> Result<CostEstimate> {
        let mut body = json!({ "query": sql, "useLegacySql": false, "dryRun": true });
        if let Some(l) = &self.location {
            body["location"] = l.clone().into();
        }
        if let Some(d) = &self.dataset {
            body["defaultDataset"] = json!({ "projectId": self.project, "datasetId": d });
        }
        if let Some(id) = self.session_id.lock().await.clone() {
            body["connectionProperties"] = json!([{ "key": "session_id", "value": id }]);
        }
        let resp = self.call(reqwest::Method::POST, format!("{}/projects/{}/queries", self.api, self.project), Some(body)).await?;
        Ok(dry_run_estimate(&resp, self.max_bytes.as_deref()))
    }

    async fn page(&self, job: &JobRef, token: Option<&str>, max: u32) -> Result<serde_json::Value> {
        let mut url =
            format!("{}/projects/{}/queries/{}?maxResults={max}&timeoutMs=10000&formatOptions.useInt64Timestamp=true", self.api, self.project, job.id);
        let loc = job.loc_param();
        if !loc.is_empty() {
            url.push('&');
            url.push_str(&loc);
        }
        if let Some(t) = token {
            url.push_str("&pageToken=");
            url.push_str(&urlencode(t));
        }
        self.call(reqwest::Method::GET, url, None).await
    }

    /// Cancel a job on the server, best effort.
    async fn cancel_job(&self, job: &JobRef) {
        let mut url = format!("{}/projects/{}/jobs/{}/cancel", self.api, self.project, job.id);
        let loc = job.loc_param();
        if !loc.is_empty() {
            url.push('?');
            url.push_str(&loc);
        }
        let _ = self.call(reqwest::Method::POST, url, None).await;
    }

    /// Wait for a job: `resp` until `jobComplete`. When waiting fails in a
    /// way that may leave the job running (the connection broke, an
    /// unexpected answer), or nobody waits for it any more, the job is
    /// cancelled on the server before returning.
    async fn wait_job(
        &self,
        job: &JobRef,
        mut resp: serde_json::Value,
        max: u32,
        cancel: &databrain_connector_core::CancellationToken,
        tx: Option<&StreamSender>,
    ) -> Result<serde_json::Value> {
        let mut backoff = Backoff::new();
        while !resp["jobComplete"].as_bool().unwrap_or(false) {
            tokio::select! {
                _ = backoff.wait() => {}
                _ = cancel.cancelled() => {}
            }
            if crate::common::abandoned(cancel, tx) {
                self.cancel_job(job).await;
                return Err(ConnectorError::cancelled());
            }
            resp = match self.page(job, None, max).await {
                Ok(r) => r,
                Err(e) => {
                    if crate::common::may_still_run(&e) {
                        self.cancel_job(job).await;
                    }
                    return Err(e);
                }
            };
        }
        Ok(resp)
    }

    async fn run_small(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let resp = self.start(sql, false, 10_000).await?;
        let job = JobRef::of(&resp, self.location.as_deref());
        let mut resp = self.wait_job(&job, resp, 10_000, &databrain_connector_core::CancellationToken::new(), None).await?;
        let mut out = Vec::new();
        loop {
            for r in resp["rows"].as_array().cloned().unwrap_or_default() {
                out.push(r["f"].as_array().cloned().unwrap_or_default().iter().map(|c| c["v"].as_str().map(str::to_string)).collect());
            }
            match resp["pageToken"].as_str().map(str::to_string) {
                Some(t) => resp = self.page(&job, Some(&t), 10_000).await?,
                None => break,
            }
        }
        Ok(out)
    }

    async fn stream(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
        let page = opts.batch_size.clamp(100, 10_000) as u32;
        let resp = self.start(sql, true, page).await?;
        let job = JobRef::of(&resp, self.location.as_deref());
        let mut resp = self.wait_job(&job, resp, page, &opts.cancel, Some(tx)).await?;
        if let Some(err) = resp.pointer("/errors/0/message").and_then(|m| m.as_str()) {
            return Err(ConnectorError::query(err.to_string()));
        }
        let fields = resp.pointer("/schema/fields").and_then(|f| f.as_array()).cloned().unwrap_or_default();
        let bytes = resp["totalBytesProcessed"].as_str().and_then(|b| b.parse::<u64>().ok());
        if fields.is_empty() {
            let n = resp["numDmlAffectedRows"].as_str().and_then(|n| n.parse().ok());
            if let Some(b) = bytes {
                tx.send(StreamEvent::Notice(format!("Bytes processed: {}", human_bytes(b)))).await;
            }
            tx.send(StreamEvent::Done(ExecSummary { rows_affected: n })).await;
            return Ok(());
        }
        let cols: Vec<Column> = fields
            .iter()
            .map(|f| {
                let t = f["type"].as_str().unwrap_or("STRING");
                let mut db = t.to_ascii_lowercase();
                if f["mode"].as_str() == Some("REPEATED") {
                    db = format!("array<{db}>");
                }
                let ct = if f["mode"].as_str() == Some("REPEATED") { ColType::Utf8 } else { col_type(t) };
                Column::new(f["name"].as_str().unwrap_or(""), ct, db)
            })
            .collect();
        let Some(mut sink) = RowSink::start(&cols, opts.batch_size, tx).await else { return Ok(()) };
        'pages: loop {
            for r in resp["rows"].as_array().cloned().unwrap_or_default() {
                let cells = r["f"].as_array().cloned().unwrap_or_default();
                let vals: Vec<Value> = sink
                    .types
                    .iter()
                    .enumerate()
                    .map(|(i, t)| bq_cell(*t, &fields[i], cells.get(i).map(|c| &c["v"]).unwrap_or(&serde_json::Value::Null)))
                    .collect();
                if !sink.push(vals).await? {
                    break 'pages;
                }
            }
            if opts.cancel.is_cancelled() {
                return Err(ConnectorError::cancelled());
            }
            match resp["pageToken"].as_str().map(str::to_string) {
                Some(t) => resp = self.page(&job, Some(&t), page).await?,
                None => break,
            }
        }
        sink.finish().await?;
        if let Some(b) = bytes {
            tx.send(StreamEvent::Notice(format!("Bytes processed: {}", human_bytes(b)))).await;
        }
        tx.send(StreamEvent::Done(ExecSummary::default())).await;
        Ok(())
    }
}

/// The estimate in a `jobs.query` dry-run response: `totalBytesProcessed`
/// (a string), `cacheHit`, and whether it is over `maximum_bytes_billed`.
fn dry_run_estimate(resp: &serde_json::Value, max_bytes: Option<&str>) -> CostEstimate {
    let num = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64());
    let bytes = num(&resp["totalBytesProcessed"]);
    let cache_hit = resp["cacheHit"].as_bool().unwrap_or(false);
    let cap = max_bytes.and_then(|m| m.trim().parse::<u64>().ok());
    let mut notes = Vec::new();
    if cache_hit {
        notes.push("The result would come from the cache: no bytes billed.".into());
    }
    let over_limit = match (bytes, cap) {
        (Some(b), Some(c)) if b > c => {
            notes.push(format!("Over this connection's maximum bytes billed ({}): the query would fail.", human_bytes(c)));
            Some(c)
        }
        _ => None,
    };
    CostEstimate { method: "dry run".into(), bytes, cache_hit, over_limit, notes, ..Default::default() }
}

fn human_bytes(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", units[i])
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `project.dataset` as two names. They go into DataBrain's own SQL between
/// backticks and into request paths, so one that could end the quoted path
/// (a backtick, a backslash) or isn't a name at all (spaces, `/`, `?`,
/// control characters) is refused: no real project or dataset id has them.
fn split_dataset(path: &str) -> Result<(&str, &str)> {
    let (project, dataset) = path.split_once('.').ok_or_else(|| ConnectorError::query(format!("expected project.dataset, got {path}")))?;
    let name =
        |n: &str| !n.is_empty() && !n.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '`' | '\\' | '/' | '?' | '#' | '\'' | '"' | ';'));
    if !name(project) || !name(dataset) {
        return Err(ConnectorError::query(format!("not a BigQuery project.dataset: {}", crate::common::truncate(path, 120))));
    }
    Ok((project, dataset))
}

#[cfg(test)]
mod dataset_path_tests {
    use super::split_dataset;

    #[test]
    fn dataset_paths_are_names() {
        assert_eq!(split_dataset("my-project.sales_2024").unwrap(), ("my-project", "sales_2024"));
        assert_eq!(split_dataset("example.com:proj.ds").unwrap(), ("example", "com:proj.ds"));
        for bad in
            ["p.d.INFORMATION_SCHEMA.TABLES`; DROP TABLE `p.d.t`; SELECT * FROM `p.d", "p.d\\", "p.d x", "p.d/../../other", "p.d?x=1", "p.", ".d", "nodot"]
        {
            assert!(split_dataset(bad).is_err(), "{bad}");
        }
    }
}

#[async_trait]
impl Session for BqSession {
    async fn bulk_load(
        &self,
        table: &str,
        cancel: databrain_connector_core::CancellationToken,
    ) -> Result<Option<Box<dyn databrain_connector_core::BulkWriter>>> {
        if !self.0.load_jobs {
            return Ok(None);
        }
        let dest = self.0.table_ref(table)?;
        Ok(Some(Box::new(LoadWriter::new(self.0.clone(), dest, cancel))))
    }

    fn bulk_load_possible(&self) -> bool {
        self.0.load_jobs
    }

    /// The table's columns from `tables.get` (no query, nothing billed).
    async fn column_types(&self, table: &str) -> Result<Option<Vec<(String, String)>>> {
        let (p, d, t) = self.0.table_ref(table)?;
        let got = self.0.call(reqwest::Method::GET, format!("{}/projects/{p}/datasets/{d}/tables/{t}", self.0.api), None).await?;
        let fields = got.pointer("/schema/fields").and_then(|f| f.as_array()).cloned().unwrap_or_default();
        Ok(Some(fields.iter().map(|f| (f["name"].as_str().unwrap_or_default().to_string(), load_type(f))).collect()))
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Bigquery
    }

    /// Every statement is its own job: a transaction can't span two of them.
    fn transaction_state(&self) -> databrain_connector_core::TxState {
        databrain_connector_core::TxState::None
    }

    async fn server_version(&self) -> Result<String> {
        Ok(format!("BigQuery (project {})", self.0.project))
    }

    async fn ping(&self) -> Result<()> {
        self.0.run_small("SELECT 1").await.map(|_| ())
    }

    async fn execute(&self, stmt: databrain_connector_core::Statement<'_>, opts: ExecOptions) -> Result<QueryStream> {
        let sql = stmt.sql();
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
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut url = format!("{API}/projects/{}/datasets?maxResults=1000", self.0.project);
            if let Some(t) = &token {
                url.push_str(&format!("&pageToken={}", urlencode(t)));
            }
            let r = self.0.call(reqwest::Method::GET, url, None).await?;
            for d in r["datasets"].as_array().cloned().unwrap_or_default() {
                let id = d.pointer("/datasetReference/datasetId").and_then(|x| x.as_str()).unwrap_or_default();
                out.push(SchemaInfo::in_catalog(self.0.project.clone(), id, self.0.dataset.as_deref() == Some(id)));
            }
            match r["nextPageToken"].as_str() {
                Some(t) => token = Some(t.to_string()),
                None => break,
            }
        }
        out.sort_by_key(|s| !s.is_default);
        Ok(out)
    }

    /// The billing project (its datasets are what `list_schemas` lists).
    async fn list_catalogs(&self) -> Result<Option<Vec<databrain_connector_core::CatalogInfo>>> {
        Ok(Some(vec![databrain_connector_core::CatalogInfo { name: self.0.project.clone(), is_default: true }]))
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let (project, dataset) = split_dataset(schema)?;
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut url = format!("{API}/projects/{project}/datasets/{dataset}/tables?maxResults=1000");
            if let Some(t) = &token {
                url.push_str(&format!("&pageToken={}", urlencode(t)));
            }
            let r = self.0.call(reqwest::Method::GET, url, None).await?;
            for t in r["tables"].as_array().cloned().unwrap_or_default() {
                let kind = match t["type"].as_str().unwrap_or("TABLE") {
                    "VIEW" => ObjectKind::View,
                    "MATERIALIZED_VIEW" => ObjectKind::MaterializedView,
                    "EXTERNAL" => ObjectKind::ForeignTable,
                    _ => ObjectKind::Table,
                };
                out.push(DbObject {
                    schema: schema.to_string(),
                    name: t.pointer("/tableReference/tableId").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                    kind,
                    comment: None,
                    row_estimate: None,
                });
            }
            match r["nextPageToken"].as_str() {
                Some(t) => token = Some(t.to_string()),
                None => break,
            }
        }
        // Routines: UDFs, table functions and procedures (best effort).
        let mut token: Option<String> = None;
        for _ in 0..20 {
            let mut url = format!("{API}/projects/{project}/datasets/{dataset}/routines?maxResults=1000");
            if let Some(t) = &token {
                url.push_str(&format!("&pageToken={}", urlencode(t)));
            }
            let Ok(r) = self.0.call(reqwest::Method::GET, url, None).await else { break };
            for t in r["routines"].as_array().cloned().unwrap_or_default() {
                let kind = if t["routineType"].as_str() == Some("PROCEDURE") { ObjectKind::Procedure } else { ObjectKind::Function };
                let name = t.pointer("/routineReference/routineId").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                if !name.is_empty() {
                    out.push(DbObject { schema: schema.to_string(), name, kind, comment: None, row_estimate: None });
                }
            }
            match r["nextPageToken"].as_str() {
                Some(t) => token = Some(t.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    async fn estimate_cost(&self, sql: &databrain_connector_core::Checked) -> Result<Option<CostEstimate>> {
        let sql = sql.sql();
        self.0.dry_run(sql).await.map(Some)
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        use databrain_connector_core::TableLayout;
        let (project, dataset) = split_dataset(schema)?;
        let t = self.0.call(reqwest::Method::GET, format!("{API}/projects/{project}/datasets/{dataset}/tables/{name}"), None).await?;
        let mut l = TableLayout::default();
        if let Some(tp) = t.get("timePartitioning") {
            l.partition_by = vec![tp["field"].as_str().unwrap_or("_PARTITIONTIME").to_string()];
            l.partition_kind = tp["type"].as_str().map(|k| k.to_ascii_lowercase());
            l.requires_partition_filter = tp["requirePartitionFilter"].as_bool().unwrap_or(false);
        } else if let Some(rp) = t.get("rangePartitioning") {
            l.partition_by = rp["field"].as_str().map(|f| vec![f.to_string()]).unwrap_or_default();
            l.partition_kind = Some("range".into());
        }
        l.requires_partition_filter |= t["requirePartitionFilter"].as_bool().unwrap_or(false);
        l.cluster_by = t
            .pointer("/clustering/fields")
            .and_then(|f| f.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        l.row_estimate = t["numRows"].as_str().and_then(|n| n.parse().ok());
        l.size_bytes = t["numBytes"].as_str().and_then(|n| n.parse().ok());
        Ok(l)
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let (project, dataset) = split_dataset(schema)?;
        let t = self.0.call(reqwest::Method::GET, format!("{API}/projects/{project}/datasets/{dataset}/tables/{name}"), None).await?;
        let kind = match t["type"].as_str().unwrap_or("TABLE") {
            "VIEW" => ObjectKind::View,
            "MATERIALIZED_VIEW" => ObjectKind::MaterializedView,
            "EXTERNAL" => ObjectKind::ForeignTable,
            _ => ObjectKind::Table,
        };
        let columns = fields_to_columns(t.pointer("/schema/fields").and_then(|f| f.as_array()).map(|v| v.as_slice()).unwrap_or(&[]));
        let ddl = t.pointer("/view/query").and_then(|q| q.as_str()).map(|q| format!("CREATE VIEW `{schema}.{name}` AS\n{q}")).or_else(|| {
            let cols: Vec<String> = columns.iter().map(|c| format!("  `{}` {}{}", c.name, c.data_type, if c.nullable { "" } else { " NOT NULL" })).collect();
            Some(format!("CREATE TABLE `{schema}.{name}` (\n{}\n);", cols.join(",\n")))
        });
        Ok(ObjectDetail {
            object: DbObject {
                schema: schema.to_string(),
                name: name.to_string(),
                kind,
                comment: t["description"].as_str().map(str::to_string),
                row_estimate: t["numRows"].as_str().and_then(|n| n.parse().ok()),
            },
            columns,
            ddl,
            foreign_keys: vec![],
        })
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let (project, dataset) = split_dataset(schema)?;
        let sql = format!(
            "SELECT table_name, column_name, data_type, is_nullable FROM `{project}.{dataset}.INFORMATION_SCHEMA.COLUMNS` ORDER BY table_name, ordinal_position"
        );
        let rows = self.0.run_small(&sql).await?;
        let mut out: Vec<TableColumns> = Vec::new();
        for r in rows {
            let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let table = g(0);
            let col = ColumnInfo { name: g(1), data_type: g(2), nullable: g(3) != "NO", is_primary_key: false, default: None, comment: None };
            match out.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => out.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        Ok(out)
    }
}

/// Connection option: loads go by one load job (newline-delimited JSON,
/// uploaded resumably), not INSERT jobs. Off by default until tried
/// against live projects.
pub const LOAD_JOBS_OPTION: &str = "load_jobs";

/// Bytes per upload request of a load job: a multiple of 256 KiB, as a
/// resumable upload's chunks must be (all but the last).
const UPLOAD_CHUNK: usize = 32 * 256 * 1024;

impl Inner {
    /// `(project, dataset, table)` of a load's table: `project.dataset.table`,
    /// `dataset.table` (this project) or `table` (the default dataset). A
    /// domain-scoped project (`example.com:project`) has a dot of its own.
    fn table_ref(&self, table: &str) -> Result<(String, String, String)> {
        let plain = table.replace('`', "");
        let mut parts: Vec<&str> = plain.rsplitn(3, '.').collect();
        parts.reverse();
        if parts.len() == 3 && parts[0].contains('.') && !parts[0].contains(':') {
            return Err(ConnectorError::query(format!("not a BigQuery table: {}", crate::common::truncate(table, 120))));
        }
        let name = |n: &str| !n.is_empty() && !n.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '\\' | '/' | '?' | '#' | '\'' | '"' | ';'));
        let r = match parts.as_slice() {
            [p, d, t] => (p.to_string(), d.to_string(), t.to_string()),
            [d, t] => (self.project.clone(), d.to_string(), t.to_string()),
            [t] => match &self.dataset {
                Some(d) => (self.project.clone(), d.clone(), t.to_string()),
                None => return Err(ConnectorError::query(format!("{table}: name its dataset (no default dataset is set)"))),
            },
            _ => return Err(ConnectorError::query(format!("not a BigQuery table: {}", crate::common::truncate(table, 120)))),
        };
        if ![&r.0, &r.1, &r.2].iter().all(|n| name(n)) {
            return Err(ConnectorError::query(format!("not a BigQuery table: {}", crate::common::truncate(table, 120))));
        }
        Ok(r)
    }

    /// Where load data is uploaded (`…/upload/bigquery/v2`).
    fn upload_api(&self) -> String {
        match self.api.strip_suffix("/bigquery/v2") {
            Some(origin) => format!("{origin}/upload/bigquery/v2"),
            None => format!("{}/upload", self.api),
        }
    }
}

/// [`Session::bulk_load`]: the chunks go up as one resumable upload, and
/// once it is complete BigQuery makes the load job (all rows or none),
/// which is waited for. Dropped unfinished, no job is made.
struct LoadWriter {
    inner: Arc<Inner>,
    dest: (String, String, String),
    job_id: String,
    /// The upload's session URL, once started.
    upload: Option<String>,
    /// Data not yet uploaded, from byte `sent` on.
    buf: Vec<u8>,
    sent: u64,
    chunk: usize,
    cancel: databrain_connector_core::CancellationToken,
}

/// What an upload request got: more wanted (bytes received so far) or the job.
enum Uploaded {
    Received(u64),
    Job(serde_json::Value),
}

impl LoadWriter {
    fn new(inner: Arc<Inner>, dest: (String, String, String), cancel: databrain_connector_core::CancellationToken) -> Self {
        let job_id = format!("databrain_load_{}", uuid::Uuid::new_v4().simple());
        LoadWriter { inner, dest, job_id, upload: None, buf: Vec::new(), sent: 0, chunk: UPLOAD_CHUNK, cancel }
    }

    /// Start the upload with the job's settings: append, the table must exist,
    /// no bad record allowed, no unknown field ignored.
    async fn start(&mut self) -> Result<()> {
        let i = &self.inner;
        let mut job_ref = json!({ "projectId": i.project, "jobId": self.job_id });
        if let Some(l) = &i.location {
            job_ref["location"] = l.clone().into();
        }
        let body = json!({
            "jobReference": job_ref,
            "configuration": { "load": {
                "destinationTable": { "projectId": self.dest.0, "datasetId": self.dest.1, "tableId": self.dest.2 },
                "sourceFormat": "NEWLINE_DELIMITED_JSON",
                "writeDisposition": "WRITE_APPEND",
                "createDisposition": "CREATE_NEVER",
                "maxBadRecords": 0,
                "ignoreUnknownValues": false,
            }},
        });
        let url = format!("{}/projects/{}/jobs?uploadType=resumable", i.upload_api(), i.project);
        for attempt in 0..2 {
            let resp = i
                .http
                .post(&url)
                .bearer_auth(i.token().await?)
                .header("X-Upload-Content-Type", "application/octet-stream")
                .json(&body)
                .send()
                .await
                .map_err(net_err)?;
            if resp.status().as_u16() == 401 && attempt == 0 {
                i.creds.invalidate().await;
                continue;
            }
            if !resp.status().is_success() {
                return json_body(resp, "BigQuery load").await.map(|_| ());
            }
            let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) else {
                return Err(ConnectorError::internal("BigQuery load: the upload started without a session URL"));
            };
            self.upload = Some(loc.to_string());
            return Ok(());
        }
        unreachable!()
    }

    /// One upload request: `len` bytes from `sent`, the last when `last`.
    async fn put(&self, len: usize, last: bool) -> Result<Uploaded> {
        let url = self.upload.clone().expect("started");
        let total = if last { (self.sent + len as u64).to_string() } else { "*".into() };
        let range = if len == 0 { format!("bytes */{total}") } else { format!("bytes {}-{}/{total}", self.sent, self.sent + len as u64 - 1) };
        let resp = self
            .inner
            .http
            .put(&url)
            .bearer_auth(self.inner.token().await?)
            .header("Content-Range", range)
            .body(self.buf[..len].to_vec())
            .send()
            .await
            .map_err(net_err)?;
        Self::uploaded(resp).await
    }

    /// How far the upload got (after a failed request).
    async fn status(&self) -> Result<Uploaded> {
        let url = self.upload.clone().expect("started");
        let resp = self
            .inner
            .http
            .put(&url)
            .bearer_auth(self.inner.token().await?)
            .header("Content-Range", "bytes */*")
            .body(Vec::new())
            .send()
            .await
            .map_err(net_err)?;
        Self::uploaded(resp).await
    }

    /// A 308 (more wanted), the job, or an error; 429 (too many requests)
    /// is a `Connection` error, retried as a 5xx is.
    async fn uploaded(resp: reqwest::Response) -> Result<Uploaded> {
        if resp.status().as_u16() == 429 {
            let e = json_body(resp, "BigQuery load").await.err().unwrap_or_else(|| ConnectorError::query("BigQuery load (429)"));
            return Err(ConnectorError::new(databrain_connector_core::ErrorKind::Connection, e.message));
        }
        if resp.status().as_u16() == 308 {
            // `Range: bytes=0-N`: N + 1 bytes received; none without it.
            let got = resp
                .headers()
                .get("range")
                .and_then(|v| v.to_str().ok())
                .and_then(|r| r.rsplit('-').next())
                .and_then(|n| n.trim().parse::<u64>().ok())
                .map_or(0, |n| n + 1);
            return Ok(Uploaded::Received(got));
        }
        json_body(resp, "BigQuery load").await.map(Uploaded::Job)
    }

    /// Upload `len` bytes of the buffer (all of it when `last`), resuming
    /// from where the server is after a dropped or failed request.
    async fn send(&mut self, len: usize, last: bool) -> Result<Option<serde_json::Value>> {
        let mut backoff = Backoff::new();
        let mut tries = 0;
        let end = self.sent + len as u64;
        loop {
            let left = (end - self.sent) as usize;
            let r = match self.put(left, last).await {
                Err(e) if e.kind == databrain_connector_core::ErrorKind::Connection && tries < 4 => {
                    tries += 1;
                    backoff.wait().await;
                    self.status().await
                }
                r => r,
            };
            match r? {
                Uploaded::Job(job) => {
                    self.buf.clear();
                    return Ok(Some(job));
                }
                Uploaded::Received(got) => {
                    if got < self.sent || got > end {
                        return Err(ConnectorError::internal(format!("BigQuery load: the upload is at byte {got}, expected {} to {end}", self.sent)));
                    }
                    self.buf.drain(..(got - self.sent) as usize);
                    self.sent = got;
                    if got == end && !last {
                        return Ok(None);
                    }
                    if got == end && left == 0 {
                        return Err(ConnectorError::internal("BigQuery load: the upload ended without a job"));
                    }
                }
            }
        }
    }

    /// Wait for the job; rows loaded. Stop cancels it.
    async fn wait(&self, job: serde_json::Value) -> Result<Option<u64>> {
        let i = &self.inner;
        let r = JobRef::of(&job, i.location.as_deref());
        let mut job = job;
        let mut backoff = Backoff::new();
        loop {
            if job.pointer("/status/state").and_then(|s| s.as_str()) == Some("DONE") {
                if let Some(e) = job.pointer("/status/errorResult") {
                    let mut msg = e["message"].as_str().unwrap_or("the load job failed").to_string();
                    let more: Vec<&str> = job
                        .pointer("/status/errors")
                        .and_then(|a| a.as_array())
                        .map(|a| a.iter().filter_map(|x| x["message"].as_str()).filter(|m| *m != msg).take(3).collect())
                        .unwrap_or_default();
                    if !more.is_empty() {
                        msg = format!("{msg} ({})", more.join("; "));
                    }
                    return Err(ConnectorError::query(format!("BigQuery load job {}: {msg}", r.id)));
                }
                return Ok(job.pointer("/statistics/load/outputRows").and_then(|n| n.as_str()).and_then(|n| n.parse().ok()));
            }
            let mut url = format!("{}/projects/{}/jobs/{}", i.api, i.project, r.id);
            let loc = r.loc_param();
            if !loc.is_empty() {
                url.push('?');
                url.push_str(&loc);
            }
            // Stop also ends a poll in flight.
            tokio::select! {
                got = async {
                    backoff.wait().await;
                    i.call(reqwest::Method::GET, url, None).await
                } => job = got?,
                _ = self.cancel.cancelled() => {
                    i.cancel_job(&r).await;
                    return Err(ConnectorError::cancelled());
                }
            }
        }
    }
}

#[async_trait]
impl databrain_connector_core::BulkWriter for LoadWriter {
    async fn write(&mut self, data: Vec<u8>) -> Result<()> {
        let cancel = self.cancel.clone();
        tokio::select! {
            r = async {
                if self.upload.is_none() {
                    self.start().await?;
                }
                self.buf.extend_from_slice(&data);
                while self.buf.len() >= self.chunk {
                    self.send(self.chunk, false).await?;
                }
                Ok(())
            } => r,
            _ = cancel.cancelled() => Err(ConnectorError::cancelled()),
        }
    }

    async fn finish(mut self: Box<Self>) -> Result<Option<u64>> {
        if self.upload.is_none() && self.buf.is_empty() {
            return Ok(Some(0));
        }
        let cancel = self.cancel.clone();
        let job = tokio::select! {
            r = async {
                if self.upload.is_none() {
                    self.start().await?;
                }
                let len = self.buf.len();
                self.send(len, true).await
            } => r?,
            _ = cancel.cancelled() => return Err(ConnectorError::cancelled()),
        };
        let job = job.ok_or_else(|| ConnectorError::internal("BigQuery load: the upload ended without a job"))?;
        self.wait(job).await
    }
}

/// A column's type as a load checks it: `ARRAY<…>` when repeated.
fn load_type(f: &serde_json::Value) -> String {
    let t = f["type"].as_str().unwrap_or("STRING").to_string();
    if f["mode"].as_str() == Some("REPEATED") { format!("ARRAY<{t}>") } else { t }
}

fn fields_to_columns(fields: &[serde_json::Value]) -> Vec<ColumnInfo> {
    fn ty(f: &serde_json::Value) -> String {
        let base = f["type"].as_str().unwrap_or("STRING");
        let t = if matches!(base, "RECORD" | "STRUCT") {
            let subs: Vec<String> =
                f["fields"].as_array().map(|a| a.iter().map(|s| format!("{} {}", s["name"].as_str().unwrap_or(""), ty(s))).collect()).unwrap_or_default();
            format!("STRUCT<{}>", subs.join(", "))
        } else {
            base.to_string()
        };
        if f["mode"].as_str() == Some("REPEATED") { format!("ARRAY<{t}>") } else { t }
    }
    fields
        .iter()
        .map(|f| ColumnInfo {
            name: f["name"].as_str().unwrap_or("").to_string(),
            data_type: ty(f),
            nullable: f["mode"].as_str() != Some("REQUIRED"),
            is_primary_key: false,
            default: None,
            comment: f["description"].as_str().map(str::to_string),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dry_run_estimates() {
        let r = json!({"totalBytesProcessed": "1073741824", "cacheHit": false, "jobComplete": true});
        let e = dry_run_estimate(&r, None);
        assert_eq!((e.method.as_str(), e.bytes, e.cache_hit, e.over_limit), ("dry run", Some(1 << 30), false, None));
        let e = dry_run_estimate(&r, Some("1000000"));
        assert_eq!(e.over_limit, Some(1_000_000));
        assert!(e.notes[0].contains("would fail"), "{:?}", e.notes);
        let e = dry_run_estimate(&json!({"totalBytesProcessed": "0", "cacheHit": true}), Some("10"));
        assert!(e.cache_hit && e.over_limit.is_none());
    }

    #[test]
    fn converts_cells() {
        let f = json!({"name": "ts", "type": "TIMESTAMP"});
        assert_eq!(bq_cell(ColType::TimestampTz, &f, &json!("1700000000000000")), Value::TimestampTz(1_700_000_000_000_000));
        let rec = json!({"name": "r", "type": "RECORD", "mode": "REPEATED", "fields": [{"name": "a", "type": "INTEGER"}, {"name": "b", "type": "STRING"}]});
        let v = json!([{"v": {"f": [{"v": "1"}, {"v": "x"}]}}]);
        assert_eq!(bq_cell(ColType::Utf8, &rec, &v), Value::Text(r#"[{"a":1,"b":"x"}]"#.into()));
    }

    #[test]
    fn column_types() {
        let cols =
            fields_to_columns(&[json!({"name": "tags", "type": "STRING", "mode": "REPEATED"}), json!({"name": "id", "type": "INTEGER", "mode": "REQUIRED"})]);
        assert_eq!(cols[0].data_type, "ARRAY<STRING>");
        assert!(!cols[1].nullable);
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }

    /// Live: DATABRAIN_BQ_PROJECT (uses gcloud ADC).
    #[ignore = "live: needs DATABRAIN_BQ_PROJECT (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(project) = std::env::var("DATABRAIN_BQ_PROJECT") else {
            panic!("live test: DATABRAIN_BQ_PROJECT not set (run with --ignored)");
        };
        use databrain_auth::{AuthMethod, MemoryStore, NonInteractive, credential_source};
        let mut c = ConnectionConfig::new(ConnectorKind::Bigquery, AuthMethod::CloudCli { profile: None });
        c.options.insert("project".into(), project);
        let creds = credential_source(&c.auth, "t", Arc::new(MemoryStore::default()), Arc::new(NonInteractive), BigQueryConnector.auth_context(&c)).unwrap();
        let s = BigQueryConnector.connect(&c, creds).await.unwrap();
        let r = s
            .execute(databrain_connector_core::Statement::internal("SELECT 1 AS a, 'x' AS b, CURRENT_TIMESTAMP() AS c, [1,2] AS d"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), 1);
    }

    fn load_inner(origin: &str) -> Inner {
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        Inner {
            project: "p1".into(),
            location: None,
            dataset: Some("ds".into()),
            max_bytes: None,
            creds: Arc::new(InlineCredentialSource::new(AuthMethod::ApiToken { user: None }, Some("tok".to_string().into()))),
            http: http(),
            session_id: Mutex::new(None),
            api: origin.to_string(),
            load_jobs: true,
        }
    }

    /// A scripted upload server: the first non-final chunk is kept only in
    /// part (`partial`), one request is answered with a status (`drop_put`:
    /// its index and status, 0 dropping the connection), and the job ends
    /// as `outcome` (its status once polled; `None`: the poll hangs).
    async fn load_server(
        partial: bool,
        drop_put: Option<(usize, u16)>,
        outcome: Option<&'static str>,
    ) -> (String, crate::mock_http::FullLog, Arc<std::sync::Mutex<Vec<u8>>>) {
        let got: Arc<std::sync::Mutex<Vec<u8>>> = Arc::default();
        let origin: Arc<std::sync::Mutex<String>> = Arc::default();
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (g, o) = (got.clone(), origin.clone());
        let (url, _, log) = crate::mock_http::serve_full(move |m, p, h, b| {
            let range = h.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-range:").map(|v| v.trim().to_string())).unwrap_or_default();
            // Before taking the lock: a hanging poll mustn't hold up the cancel.
            if m == "GET" && p == "/projects/p1/jobs/j9?location=EU" {
                match outcome {
                    Some(o) => return (200, vec![], o.to_string()),
                    None => tokio::task::block_in_place(|| std::thread::sleep(std::time::Duration::from_secs(4))),
                }
            }
            let mut got = g.lock().unwrap();
            let received = |n: usize| if n == 0 { vec![] } else { vec![("Range".to_string(), format!("bytes=0-{}", n - 1))] };
            if m == "POST" && p == "/upload/projects/p1/jobs?uploadType=resumable" {
                return (200, vec![("Location".into(), format!("{}/up/1", o.lock().unwrap()))], String::new());
            }
            if m == "PUT" && p == "/up/1" {
                if range == "bytes */*" {
                    return (308, received(got.len()), String::new());
                }
                let k = puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some((at, status)) = drop_put
                    && at == k
                {
                    return (status, vec![], json!({"error": {"message": "Too many requests"}}).to_string());
                }
                let spec = range.trim_start_matches("bytes ");
                let (span, total) = spec.split_once('/').unwrap();
                if span != "*" {
                    let start: usize = span.split('-').next().unwrap().parse().unwrap();
                    assert_eq!(start, got.len(), "a chunk from where the server is");
                    let mut body = b.as_bytes().to_vec();
                    if partial && k == 0 && total == "*" {
                        body.truncate(body.len() / 2);
                    }
                    got.extend_from_slice(&body);
                }
                if total != "*" && total.parse::<usize>().unwrap() == got.len() {
                    return (
                        200,
                        vec![],
                        json!({"jobReference": {"projectId": "p1", "jobId": "j9", "location": "EU"}, "status": {"state": "RUNNING"}}).to_string(),
                    );
                }
                return (308, received(got.len()), String::new());
            }
            (404, vec![], "{}".into())
        })
        .await;
        *origin.lock().unwrap() = url.clone();
        (url, log, got)
    }

    async fn load(inner: Inner, chunks: &[&str], chunk: usize) -> Result<Option<u64>> {
        use databrain_connector_core::BulkWriter;
        let inner = Arc::new(inner);
        let dest = inner.table_ref("ds.t").unwrap();
        let mut w = Box::new(LoadWriter::new(inner, dest, Default::default()));
        w.chunk = chunk;
        for c in chunks {
            w.write(c.as_bytes().to_vec()).await?;
        }
        w.finish().await
    }

    const DONE_3: &str = r#"{"status": {"state": "DONE"}, "statistics": {"load": {"outputRows": "3"}}}"#;

    /// The rows go up in chunks of one upload (resent from where the server
    /// is when it kept only part of one), then the job is waited for, where
    /// it runs, and its row count is the load's.
    #[tokio::test]
    async fn a_load_job_uploads_its_rows_then_waits_for_the_job() {
        let (url, log, got) = load_server(true, None, Some(DONE_3)).await;
        let rows = ["{\"a\":1}\n", "{\"a\":2}\n", "{\"a\":3}\n"];
        assert_eq!(load(load_inner(&url), &rows, 8).await.unwrap(), Some(3));
        assert_eq!(String::from_utf8(got.lock().unwrap().clone()).unwrap(), rows.concat());
        let log = log.lock().unwrap();
        let start: serde_json::Value = serde_json::from_str(&log[0].3).unwrap();
        let cfg = &start["configuration"]["load"];
        assert_eq!(cfg["destinationTable"], json!({"projectId": "p1", "datasetId": "ds", "tableId": "t"}));
        assert_eq!(
            (cfg["sourceFormat"].as_str(), cfg["writeDisposition"].as_str(), cfg["createDisposition"].as_str()),
            (Some("NEWLINE_DELIMITED_JSON"), Some("WRITE_APPEND"), Some("CREATE_NEVER"))
        );
        assert!(start["jobReference"]["jobId"].as_str().unwrap().starts_with("databrain_load_"));
        assert!(log.iter().any(|(m, p, _, _)| m == "GET" && p == "/projects/p1/jobs/j9?location=EU"));
        // Every upload request carries the token, not only the first.
        for (m, _, h, _) in log.iter().filter(|(m, ..)| m == "PUT") {
            assert!(h.lines().any(|l| l.eq_ignore_ascii_case("authorization: Bearer tok")), "{m}: {h}");
        }
    }

    /// Stop while the job's status is being asked for ends the load at once
    /// (the poll in flight isn't waited for).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stop_ends_a_poll_in_flight() {
        use databrain_connector_core::BulkWriter;
        let (url, _, _) = load_server(false, None, None).await;
        let inner = Arc::new(load_inner(&url));
        let dest = inner.table_ref("ds.t").unwrap();
        let cancel = databrain_connector_core::CancellationToken::new();
        let mut w = Box::new(LoadWriter::new(inner, dest, cancel.clone()));
        w.write(b"{\"a\":1}\n".to_vec()).await.unwrap();
        let stop = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            cancel.cancel();
        });
        let e = tokio::time::timeout(std::time::Duration::from_secs(3), w.finish()).await.expect("stopped, not waiting for the poll").unwrap_err();
        assert!(e.is_cancelled(), "{e:?}");
        stop.await.unwrap();
    }

    /// A dropped upload request, or one answered 429 or 5xx: the writer asks
    /// where the upload is and goes on from there.
    #[tokio::test]
    async fn a_dropped_upload_request_resumes() {
        for drop in [(0, 0), (2, 0), (1, 429), (0, 503)] {
            let (url, _, got) = load_server(false, Some(drop), Some(DONE_3)).await;
            let rows = ["{\"a\":1}\n", "{\"a\":2}\n", "{\"a\":3}\n"];
            assert_eq!(load(load_inner(&url), &rows, 8).await.unwrap(), Some(3), "drop {drop:?}");
            assert_eq!(String::from_utf8(got.lock().unwrap().clone()).unwrap(), rows.concat(), "drop {drop:?}");
        }
    }

    /// A job that fails says why (its first errors).
    #[tokio::test]
    async fn a_failed_load_job_says_why() {
        let failed = r#"{"status": {"state": "DONE", "errorResult": {"message": "Error while reading data"}, "errors": [{"message": "Error while reading data"}, {"message": "Could not parse 'x' as INT64 for field a"}]}}"#;
        let (url, _, _) = load_server(false, None, Some(failed)).await;
        let e = load(load_inner(&url), &["{\"a\":\"x\"}\n"], 8).await.unwrap_err();
        assert!(e.message.contains("Error while reading data (Could not parse 'x' as INT64 for field a)"), "{}", e.message);
    }

    /// No rows: no upload and no job.
    #[tokio::test]
    async fn an_empty_load_makes_no_job() {
        let (url, log, _) = load_server(false, None, Some(DONE_3)).await;
        assert_eq!(load(load_inner(&url), &[], 8).await.unwrap(), Some(0));
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn load_tables_are_named_in_full() {
        let i = load_inner("http://x");
        assert_eq!(i.table_ref("`p2.d.t`").unwrap(), ("p2".into(), "d".into(), "t".into()));
        assert_eq!(i.table_ref("d.t").unwrap(), ("p1".into(), "d".into(), "t".into()));
        assert_eq!(i.table_ref("t").unwrap(), ("p1".into(), "ds".into(), "t".into()));
        assert!(i.table_ref("a.b.c.d").is_err());
        // A domain-scoped project has a dot of its own.
        assert_eq!(i.table_ref("`example.com:p3.d.t`").unwrap(), ("example.com:p3".into(), "d".into(), "t".into()));
        assert!(i.table_ref("d.t?x").is_err());
        assert_eq!(i.upload_api(), "http://x/upload");
        assert_eq!(Inner { api: API.into(), ..load_inner("") }.upload_api(), "https://bigquery.googleapis.com/upload/bigquery/v2");
    }

    /// A job running in `asia-southeast1` on a connection set to `US`: the
    /// first poll gets `poll` (status 0: the connection drops).
    async fn running_job(poll: (u16, &'static str)) -> (Inner, crate::mock_http::Log) {
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let (origin, log) = crate::mock_http::serve(move |m, p, _| {
            if m == "POST" && p == "/projects/p1/queries" {
                (200, json!({"jobReference": {"projectId": "p1", "jobId": "j1", "location": "asia-southeast1"}, "jobComplete": false}).to_string())
            } else if m == "GET" && p.starts_with("/projects/p1/queries/j1?") {
                (poll.0, poll.1.to_string())
            } else if m == "POST" && p.starts_with("/projects/p1/jobs/j1/cancel") {
                (200, "{}".into())
            } else {
                (404, "{}".into())
            }
        })
        .await;
        let inner = Inner {
            project: "p1".into(),
            location: Some("US".into()),
            dataset: None,
            max_bytes: None,
            creds: Arc::new(InlineCredentialSource::new(AuthMethod::ApiToken { user: None }, Some("tok".to_string().into()))),
            http: http(),
            session_id: Mutex::new(None),
            api: origin,
            load_jobs: false,
        };
        (inner, log)
    }

    fn cancels(log: &crate::mock_http::Log) -> Vec<String> {
        log.lock().unwrap().iter().filter(|(m, p, _)| m == "POST" && p.contains("/jobs/j1/cancel")).map(|(_, p, _)| p.clone()).collect()
    }

    /// A poll that fails without the server saying how the job ended: the
    /// job is cancelled, where it runs (its own location, not the connection's).
    #[tokio::test]
    async fn a_failed_poll_cancels_the_job_where_it_runs() {
        for poll in [(0, ""), (503, "<html>unavailable</html>")] {
            let (i, log) = running_job(poll).await;
            assert!(i.run_small("select 1").await.is_err());
            let polls: Vec<String> = log.lock().unwrap().iter().filter(|(m, _, _)| m == "GET").map(|(_, p, _)| p.clone()).collect();
            assert!(polls.iter().all(|p| p.ends_with("&location=asia-southeast1")), "{polls:?}");
            assert_eq!(cancels(&log), ["/projects/p1/jobs/j1/cancel?location=asia-southeast1"], "{poll:?}");
        }
    }

    /// The server said how the job ended (failed, or done): nothing to cancel.
    #[tokio::test]
    async fn a_job_that_ended_is_not_cancelled() {
        let failed: &'static str = r#"{"error": {"code": 400, "message": "Syntax error: Unexpected end of script"}}"#;
        let done: &'static str = r#"{"jobComplete": true, "rows": [], "schema": {"fields": []}}"#;
        for (poll, ok) in [((400, failed), false), ((200, done), true)] {
            let (i, log) = running_job(poll).await;
            assert_eq!(i.run_small("select 1").await.is_ok(), ok, "{poll:?}");
            assert!(cancels(&log).is_empty(), "{poll:?}");
        }
    }

    /// Nobody reads the rows any more (the stream dropped): the job is cancelled.
    #[tokio::test]
    async fn a_dropped_stream_cancels_the_job() {
        let (i, log) = running_job((200, r#"{"jobComplete": false}"#)).await;
        let (tx, stream) = databrain_connector_core::QueryStream::channel(1);
        drop(stream);
        let job = JobRef { id: "j1".into(), location: Some("asia-southeast1".into()) };
        let err = i.wait_job(&job, json!({"jobComplete": false}), 100, &databrain_connector_core::CancellationToken::new(), Some(&tx)).await.unwrap_err();
        assert_eq!(err.kind, databrain_connector_core::ErrorKind::Cancelled);
        assert_eq!(cancels(&log).len(), 1);
    }
}
