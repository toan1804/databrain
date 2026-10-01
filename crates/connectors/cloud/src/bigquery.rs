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
    Capabilities, ColType, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind,
    DbObject, ExecOptions, ExecSummary, FieldSpec, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo,
    Session, StreamEvent, StreamSender, TableColumns, Value,
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
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Bigquery,
            display_name: "BigQuery",
            default_port: None,
            uses_file: false,
            auth_methods: vec![AuthMethodKind::OauthBrowser, AuthMethodKind::ServiceAccount, AuthMethodKind::CloudCli],
            capabilities: Capabilities { transactions: true, cancel: true, schemas: true, read_only_sessions: false, ssh: false },
            fields: vec![
                FieldSpec::new("project", "Billing project ID").required().placeholder("my-gcp-project"),
                FieldSpec::new("location", "Location").placeholder("US / EU / asia-southeast1"),
                FieldSpec::new("dataset", "Default dataset").placeholder("(optional)"),
                FieldSpec::new("maximum_bytes_billed", "Max bytes billed per query").placeholder("(optional) e.g. 10000000000"),
            ],
            note: Some("Browser sign-in needs a Google OAuth client ID (Desktop app). Service accounts need BigQuery Job User."),
        }
    }

    fn auth_context(&self, cfg: &ConnectionConfig) -> AuthContext {
        let p = cfg.auth.oauth_params().cloned().unwrap_or_default();
        let scopes: Vec<String> = p
            .scopes
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|| SCOPES.iter().map(|s| s.to_string()).collect());
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
        return J::Array(
            v.as_array()
                .map(|a| a.iter().map(|x| plain_json(&elem_field, x.get("v").unwrap_or(x))).collect())
                .unwrap_or_default(),
        );
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

    fn loc_param(&self) -> String {
        self.location.as_ref().map(|l| format!("&location={l}")).unwrap_or_default()
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
        let resp = self.call(reqwest::Method::POST, format!("{API}/projects/{}/queries", self.project), Some(body)).await?;
        if in_session {
            if let Some(id) = resp.pointer("/sessionInfo/sessionId").and_then(|s| s.as_str()) {
                *self.session_id.lock().await = Some(id.to_string());
            }
        }
        Ok(resp)
    }

    async fn page(&self, job_id: &str, token: Option<&str>, max: u32) -> Result<serde_json::Value> {
        let mut url = format!(
            "{API}/projects/{}/queries/{job_id}?maxResults={max}&timeoutMs=10000&formatOptions.useInt64Timestamp=true{}",
            self.project,
            self.loc_param()
        );
        if let Some(t) = token {
            url.push_str("&pageToken=");
            url.push_str(&urlencode(t));
        }
        self.call(reqwest::Method::GET, url, None).await
    }

    async fn run_small(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let mut resp = self.start(sql, false, 10_000).await?;
        let job_id = resp.pointer("/jobReference/jobId").and_then(|j| j.as_str()).unwrap_or_default().to_string();
        let mut backoff = Backoff::new();
        while !resp["jobComplete"].as_bool().unwrap_or(false) {
            backoff.wait().await;
            resp = self.page(&job_id, None, 10_000).await?;
        }
        let mut out = Vec::new();
        loop {
            for r in resp["rows"].as_array().cloned().unwrap_or_default() {
                out.push(r["f"].as_array().cloned().unwrap_or_default().iter().map(|c| c["v"].as_str().map(str::to_string)).collect());
            }
            match resp["pageToken"].as_str().map(str::to_string) {
                Some(t) => resp = self.page(&job_id, Some(&t), 10_000).await?,
                None => break,
            }
        }
        Ok(out)
    }

    async fn stream(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
        let page = opts.batch_size.clamp(100, 10_000) as u32;
        let mut resp = self.start(sql, true, page).await?;
        let job_id = resp.pointer("/jobReference/jobId").and_then(|j| j.as_str()).unwrap_or_default().to_string();
        let mut backoff = Backoff::new();
        while !resp["jobComplete"].as_bool().unwrap_or(false) {
            tokio::select! {
                _ = backoff.wait() => {}
                _ = opts.cancel.cancelled() => {
                    let _ = self
                        .call(reqwest::Method::POST, format!("{API}/projects/{}/jobs/{job_id}/cancel?{}", self.project, self.loc_param().trim_start_matches('&')), None)
                        .await;
                    return Err(ConnectorError::cancelled());
                }
            }
            resp = self.page(&job_id, None, page).await?;
        }
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
                Some(t) => resp = self.page(&job_id, Some(&t), page).await?,
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

fn split_dataset(path: &str) -> Result<(&str, &str)> {
    path.split_once('.').ok_or_else(|| ConnectorError::query(format!("expected project.dataset, got {path}")))
}

#[async_trait]
impl Session for BqSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Bigquery
    }

    async fn server_version(&self) -> Result<String> {
        Ok(format!("BigQuery (project {})", self.0.project))
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
        Ok(out)
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let (project, dataset) = split_dataset(schema)?;
        let t = self
            .0
            .call(reqwest::Method::GET, format!("{API}/projects/{project}/datasets/{dataset}/tables/{name}"), None)
            .await?;
        let kind = match t["type"].as_str().unwrap_or("TABLE") {
            "VIEW" => ObjectKind::View,
            "MATERIALIZED_VIEW" => ObjectKind::MaterializedView,
            "EXTERNAL" => ObjectKind::ForeignTable,
            _ => ObjectKind::Table,
        };
        let columns = fields_to_columns(t.pointer("/schema/fields").and_then(|f| f.as_array()).map(|v| v.as_slice()).unwrap_or(&[]));
        let ddl = t
            .pointer("/view/query")
            .and_then(|q| q.as_str())
            .map(|q| format!("CREATE VIEW `{schema}.{name}` AS\n{q}"))
            .or_else(|| {
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

fn fields_to_columns(fields: &[serde_json::Value]) -> Vec<ColumnInfo> {
    fn ty(f: &serde_json::Value) -> String {
        let base = f["type"].as_str().unwrap_or("STRING");
        let t = if matches!(base, "RECORD" | "STRUCT") {
            let subs: Vec<String> = f["fields"]
                .as_array()
                .map(|a| a.iter().map(|s| format!("{} {}", s["name"].as_str().unwrap_or(""), ty(s))).collect())
                .unwrap_or_default();
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
    fn converts_cells() {
        let f = json!({"name": "ts", "type": "TIMESTAMP"});
        assert_eq!(bq_cell(ColType::TimestampTz, &f, &json!("1700000000000000")), Value::TimestampTz(1_700_000_000_000_000));
        let rec = json!({"name": "r", "type": "RECORD", "mode": "REPEATED", "fields": [{"name": "a", "type": "INTEGER"}, {"name": "b", "type": "STRING"}]});
        let v = json!([{"v": {"f": [{"v": "1"}, {"v": "x"}]}}]);
        assert_eq!(bq_cell(ColType::Utf8, &rec, &v), Value::Text(r#"[{"a":1,"b":"x"}]"#.into()));
    }

    #[test]
    fn column_types() {
        let cols = fields_to_columns(&[json!({"name": "tags", "type": "STRING", "mode": "REPEATED"}), json!({"name": "id", "type": "INTEGER", "mode": "REQUIRED"})]);
        assert_eq!(cols[0].data_type, "ARRAY<STRING>");
        assert!(!cols[1].nullable);
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }

    /// Live: DATABRAIN_BQ_PROJECT (uses gcloud ADC).
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(project) = std::env::var("DATABRAIN_BQ_PROJECT") else {
            eprintln!("skipping: DATABRAIN_BQ_PROJECT not set");
            return;
        };
        use databrain_auth::{AuthMethod, MemoryStore, NonInteractive, credential_source};
        let mut c = ConnectionConfig::new(ConnectorKind::Bigquery, AuthMethod::CloudCli { profile: None });
        c.options.insert("project".into(), project);
        let creds = credential_source(&c.auth, "t", Arc::new(MemoryStore::default()), Arc::new(NonInteractive), BigQueryConnector.auth_context(&c)).unwrap();
        let s = BigQueryConnector.connect(&c, creds).await.unwrap();
        let r = s.execute("SELECT 1 AS a, 'x' AS b, CURRENT_TIMESTAMP() AS c, [1,2] AS d", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), 1);
    }
}
