//! ClickHouse connector over the HTTP interface (port 8123; 8443 with TLS).
//!
//! Each DataBrain session is a ClickHouse HTTP session (`session_id`), so
//! `SET`, temporary tables and `USE` carry over between statements. Results
//! are read as `TabSeparatedWithNamesAndTypes` and streamed line by line.
//! Metadata comes from the `system` tables, outside the session, so the
//! explorer never waits for a running query.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, Credential, CredentialSource};
use databrain_connector_core::value::{parse_date, parse_datetime};
use databrain_connector_core::{
    BatchBuilder, Capabilities, ColType, Column, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, DbObject, ErrorKind,
    ExecOptions, ExecSummary, FieldSpec, IndexInfo, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session, SslMode, StreamEvent, StreamSender,
    TableColumns, TableLayout, Value, split_key_list,
};
use secrecy::ExposeSecret;

const HTTP_PORT: u16 = 8123;
const HTTPS_PORT: u16 = 8443;
const FORMAT: &str = "TabSeparatedWithNamesAndTypes";
/// Seconds an idle ClickHouse session is kept (the server caps it at `max_session_timeout`, 3600 by default).
const SESSION_TIMEOUT: &str = "3600";

#[derive(Debug, Default)]
pub struct ClickhouseConnector;

impl ClickhouseConnector {
    pub fn new() -> Self {
        Self
    }
}

/// `http(s)://host:port/` for a config, and whether certificates are checked.
fn endpoint(cfg: &ConnectionConfig) -> (String, bool) {
    let raw = cfg.host_or_default().trim().trim_end_matches('/').to_string();
    let (forced_tls, host) = match raw.strip_prefix("https://") {
        Some(h) => (Some(true), h.to_string()),
        None => match raw.strip_prefix("http://") {
            Some(h) => (Some(false), h.to_string()),
            None => (None, raw),
        },
    };
    let tls = forced_tls.unwrap_or(match cfg.ssl_mode {
        SslMode::Disable => false,
        // "Prefer": TLS on the standard TLS ports (ClickHouse Cloud is 8443).
        SslMode::Prefer => matches!(cfg.port, Some(HTTPS_PORT) | Some(443)),
        SslMode::Require | SslMode::VerifyFull => true,
    });
    let port = cfg.port.unwrap_or(if tls { HTTPS_PORT } else { HTTP_PORT });
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
    (format!("{}://{host}:{port}/", if tls { "https" } else { "http" }), tls && cfg.ssl_mode == SslMode::Require)
}

#[async_trait]
impl Connector for ClickhouseConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Clickhouse,
            traits: ConnectorKind::Clickhouse.traits(),
            display_name: "ClickHouse",
            default_port: Some(HTTP_PORT),
            uses_file: false,
            auth_methods: vec![AuthMethodKind::Password],
            capabilities: Capabilities { transactions: false, cancel: true, schemas: true, read_only_sessions: true, ssh: true },
            fields: vec![
                FieldSpec::new("host", "Host").required().placeholder("localhost"),
                FieldSpec::new("port", "HTTP port").placeholder("8123").help("The HTTP interface: 8123, or 8443 with TLS (ClickHouse Cloud)"),
                FieldSpec::new("database", "Database").placeholder("default"),
            ],
            note: None,
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let (user, password) = match creds.get().await? {
            Credential::Password { user, password } => (user, password.expose_secret().to_string()),
            _ => return Err(ConnectorError::config("ClickHouse requires user/password authentication")),
        };
        let (base, accept_invalid) = endpoint(cfg);
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(50))
            .danger_accept_invalid_certs(accept_invalid)
            .user_agent(concat!("DataBrain/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ConnectorError::internal(e.to_string()))?;
        let inner = Arc::new(Inner {
            http,
            base,
            user: if user.trim().is_empty() { "default".into() } else { user },
            password,
            session_id: uuid::Uuid::new_v4().to_string(),
            read_only: cfg.read_only,
            database: std::sync::Mutex::new(cfg.database.clone().filter(|d| !d.trim().is_empty())),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        // Checks the address, the sign-in and the database at once.
        let current = tokio::time::timeout(Duration::from_secs(20), inner.rows("SELECT currentDatabase()", false))
            .await
            .map_err(|_| ConnectorError::connection("connection timed out"))?
            .map_err(|mut e| {
                if e.kind == ErrorKind::Query && e.code.is_none() {
                    e.kind = ErrorKind::Connection;
                }
                e
            })?;
        if let Some(db) = current.first().and_then(|r| r.first().cloned().flatten()) {
            *inner.database.lock().unwrap() = Some(db);
        }
        Ok(Box::new(ChSession(inner)))
    }
}

struct Inner {
    http: reqwest::Client,
    base: String,
    user: String,
    password: String,
    session_id: String,
    read_only: bool,
    /// Current database (sent with each request; `USE` changes it).
    database: std::sync::Mutex<Option<String>>,
    /// ClickHouse runs one query at a time per session.
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Inner {
    fn request(&self, sql: &str, query_id: &str, in_session: bool) -> reqwest::RequestBuilder {
        let mut q: Vec<(&str, String)> = vec![("query_id", query_id.to_string()), ("default_format", FORMAT.into())];
        if in_session {
            q.push(("session_id", self.session_id.clone()));
            q.push(("session_timeout", SESSION_TIMEOUT.into()));
        }
        if let Some(db) = self.database.lock().unwrap().clone() {
            q.push(("database", db));
        }
        if self.read_only {
            // Read and SET only; enforced by the server.
            q.push(("readonly", "2".into()));
        }
        self.http.post(&self.base).query(&q).header("X-ClickHouse-User", &self.user).header("X-ClickHouse-Key", &self.password).body(sql.to_string())
    }

    async fn send(&self, sql: &str, query_id: &str, in_session: bool) -> Result<reqwest::Response> {
        // A cancelled query can hold the session for a moment after KILL.
        let mut tries = 0;
        loop {
            match self.send_once(sql, query_id, in_session).await {
                Err(e) if in_session && tries < 40 && e.code.as_deref().is_some_and(|c| c.starts_with("373 ")) => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                r => return r,
            }
        }
    }

    async fn send_once(&self, sql: &str, query_id: &str, in_session: bool) -> Result<reqwest::Response> {
        let resp = self.request(sql, query_id, in_session).send().await.map_err(net_err)?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let code = resp.headers().get("X-ClickHouse-Exception-Code").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u32>().ok());
        let text = resp.text().await.unwrap_or_default();
        Err(server_error(code, status.as_u16(), &text))
    }

    /// Ask the server to stop a query (from outside its session, which is busy).
    async fn kill(&self, query_id: &str) {
        let sql = format!("KILL QUERY WHERE query_id = {} ASYNC", lit(query_id));
        let id = uuid::Uuid::new_v4().to_string();
        if let Ok(r) = self.request(&sql, &id, false).send().await {
            let _ = r.bytes().await;
        }
    }

    /// A small query read whole (metadata), outside the session unless `in_session`.
    async fn rows(&self, sql: &str, in_session: bool) -> Result<Vec<Vec<Option<String>>>> {
        let id = uuid::Uuid::new_v4().to_string();
        let _guard = if in_session { Some(self.lock.lock().await) } else { None };
        let body = self.send(sql, &id, in_session).await?.bytes().await.map_err(net_err)?;
        let mut lines = body.split(|b| *b == b'\n').filter(|l| !l.is_empty());
        // Names and types.
        lines.next();
        lines.next();
        let mut out = Vec::new();
        for l in lines {
            if l.starts_with(b"Code: ") || l.starts_with(b"__exception__") {
                return Err(server_error(None, 500, &String::from_utf8_lossy(l)));
            }
            out.push(split_tsv(l));
        }
        Ok(out)
    }

    async fn stream(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
        let _guard = self.lock.clone().lock_owned().await;
        let query_id = uuid::Uuid::new_v4().to_string();
        let mut resp = tokio::select! {
            r = self.send(sql, &query_id, true) => r?,
            _ = opts.cancel.cancelled() => {
                self.kill(&query_id).await;
                return Err(ConnectorError::cancelled());
            }
        };
        let written = resp
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v["written_rows"].as_str().and_then(|n| n.parse::<u64>().ok()))
            .filter(|n| *n > 0);

        let mut buf: Vec<u8> = Vec::new();
        let mut header: Vec<Vec<Option<String>>> = Vec::new();
        let mut sink: Option<(BatchBuilder, Vec<ColType>)> = None;
        let mut exception: Option<String> = None;
        let max = opts.max_rows.unwrap_or(usize::MAX);
        let mut rows = 0usize;
        'read: loop {
            let chunk = tokio::select! {
                c = resp.chunk() => c.map_err(|e| ended_early(e, &query_id, rows))?,
                _ = opts.cancel.cancelled() => {
                    drop(resp);
                    self.kill(&query_id).await;
                    return Err(ConnectorError::cancelled());
                }
            };
            let done = chunk.is_none();
            if let Some(c) = chunk {
                buf.extend_from_slice(&c);
            } else if !buf.is_empty() && buf.last() != Some(&b'\n') {
                buf.push(b'\n');
            }
            while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=i).collect();
                let line = &line[..line.len() - 1];
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                // An error after the result started (the HTTP status was already 200).
                if let Some(e) = exception.as_mut() {
                    if line.starts_with(b"__exception__") {
                        break 'read;
                    }
                    if !e.is_empty() {
                        e.push('\n');
                    }
                    e.push_str(&String::from_utf8_lossy(line));
                    continue;
                }
                if line.starts_with(b"__exception__") {
                    exception = Some(String::new());
                    continue;
                }
                let width = header.first().map(Vec::len);
                if line.starts_with(b"Code: ") && width.is_some_and(|w| line.split(|b| *b == b'\t').count() != w) {
                    exception = Some(String::from_utf8_lossy(line).into_owned());
                    continue;
                }
                if header.len() < 2 {
                    header.push(split_tsv(line));
                    if header.len() == 2 {
                        let cols: Vec<Column> = header[0]
                            .iter()
                            .zip(&header[1])
                            .map(|(n, t)| {
                                let t = t.clone().unwrap_or_default();
                                Column::new(n.clone().unwrap_or_default(), col_type(&t), t)
                            })
                            .collect();
                        let b = BatchBuilder::new(&cols, opts.batch_size.max(1));
                        if !tx.send(StreamEvent::Schema(b.schema())).await {
                            return Ok(());
                        }
                        sink = Some((b, cols.iter().map(|c| c.col_type).collect()));
                    }
                    continue;
                }
                let Some((b, types)) = sink.as_mut() else { continue };
                let cells = split_tsv(line);
                b.push_row(types.iter().enumerate().map(|(i, t)| cell(*t, cells.get(i).cloned().flatten())));
                rows += 1;
                if b.is_full() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
                    return Ok(());
                }
                if rows >= max {
                    // Dropping the response stops a read-only query on the server.
                    break 'read;
                }
            }
            if done {
                break;
            }
        }
        if let Some(e) = exception {
            return Err(server_error(None, 500, e.trim()));
        }
        if let Some((mut b, _)) = sink {
            if !b.is_empty() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
                return Ok(());
            }
            if b.coercion_failures() > 0 {
                tx.send(StreamEvent::Notice(format!("{} value(s) could not be converted and are shown as NULL", b.coercion_failures()))).await;
            }
        } else if !header.is_empty() {
            return Err(ConnectorError::internal("ClickHouse: the result ended after its column names"));
        }
        if let Some(db) = use_target(sql) {
            *self.database.lock().unwrap() = Some(db);
        }
        tx.send(StreamEvent::Done(ExecSummary { rows_affected: written })).await;
        Ok(())
    }
}

/// The result broke off after it started: ClickHouse does that when the
/// query fails midway, and its message doesn't always come with it.
fn ended_early(e: reqwest::Error, query_id: &str, rows: usize) -> ConnectorError {
    let mut err = net_err(e);
    err.message = format!(
        "ClickHouse stopped sending the result after {rows} rows, usually because the query failed on the server; its error is in system.query_log (query_id = '{query_id}'). ({})",
        err.message
    );
    err
}

fn net_err(e: reqwest::Error) -> ConnectorError {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        let t = s.to_string();
        if !msg.contains(&t) {
            msg.push_str(": ");
            msg.push_str(&t);
        }
        src = s.source();
    }
    ConnectorError::connection(msg)
}

/// `Code: 60. DB::Exception: Table x doesn't exist. (UNKNOWN_TABLE) (version 24.3.1)` → error.
fn server_error(code: Option<u32>, status: u16, text: &str) -> ConnectorError {
    let text = text.trim();
    let code = code.or_else(|| text.strip_prefix("Code: ").and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok()));
    let mut msg = text;
    if let Some(i) = msg.find("DB::Exception: ") {
        msg = &msg[i + "DB::Exception: ".len()..];
    }
    if let Some(i) = msg.rfind(" (version ") {
        msg = &msg[..i];
    }
    // The error name: `(UNKNOWN_TABLE)` at the end.
    let name = msg
        .strip_suffix(')')
        .and_then(|m| m.rfind(" (").map(|i| (&m[i + 2..], &m[..i])))
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()));
    let (name, msg) = match name {
        Some((n, m)) => (Some(n.to_string()), m.trim_end_matches('.').to_string() + "."),
        None => (None, msg.to_string()),
    };
    let kind = match (code, status) {
        (Some(394), _) => return ConnectorError::cancelled(),
        (Some(516 | 192 | 193 | 194 | 195 | 497), _) | (_, 401 | 403) => ErrorKind::Auth,
        (None, 502..=504) => ErrorKind::Connection,
        _ => ErrorKind::Query,
    };
    let mut e = ConnectorError::new(kind, if msg.is_empty() { format!("ClickHouse error (HTTP {status})") } else { msg.clone() });
    if let Some(c) = code {
        e = e.with_code(match name {
            Some(n) => format!("{c} ({n})"),
            None => c.to_string(),
        });
    }
    // Syntax errors: "failed at position 8 (…)" (1-based).
    if let Some(p) = msg.split("failed at position ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse::<u32>().ok()) {
        e = e.with_position(Some(p));
    }
    e
}

/// The database a `USE db` statement switches to.
fn use_target(sql: &str) -> Option<String> {
    let s = sql.trim().trim_end_matches(';').trim();
    let (word, rest) = s.split_once(char::is_whitespace)?;
    if !word.eq_ignore_ascii_case("use") {
        return None;
    }
    let name = rest.trim();
    let quoted = name
        .strip_prefix('`')
        .and_then(|n| n.strip_suffix('`'))
        .or_else(|| name.strip_prefix('"').and_then(|n| n.strip_suffix('"')))
        .map(|n| n.replace("\\`", "`").replace("\\\"", "\""));
    match quoted {
        Some(q) => (!q.is_empty()).then_some(q),
        None => (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_string()),
    }
}

/// A string literal (backslash escapes).
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The type inside `Nullable(…)` / `LowCardinality(…)`.
fn base_type(t: &str) -> &str {
    let mut t = t.trim();
    loop {
        let inner = ["Nullable(", "LowCardinality("].iter().find_map(|w| t.strip_prefix(w).and_then(|r| r.strip_suffix(')')));
        match inner {
            Some(i) => t = i.trim(),
            None => return t,
        }
    }
}

fn col_type(t: &str) -> ColType {
    let b = base_type(t);
    match b.split('(').next().unwrap_or(b) {
        "Bool" => ColType::Bool,
        "Int8" | "Int16" | "Int32" | "Int64" | "UInt8" | "UInt16" | "UInt32" => ColType::Int64,
        "UInt64" => ColType::UInt64,
        "Float32" | "Float64" | "BFloat16" => ColType::Float64,
        "Date" | "Date32" => ColType::Date,
        "DateTime" | "DateTime64" => ColType::Timestamp,
        // Decimals and 128/256-bit integers stay text to keep every digit.
        _ => ColType::Utf8,
    }
}

fn cell(t: ColType, s: Option<String>) -> Value {
    let Some(s) = s else { return Value::Null };
    match t {
        ColType::Bool => match s.as_str() {
            "true" | "1" => Value::Bool(true),
            "false" | "0" => Value::Bool(false),
            _ => Value::Text(s),
        },
        ColType::Int64 => s.parse().map(Value::Int).unwrap_or(Value::Text(s)),
        ColType::UInt64 => s.parse().map(Value::UInt).unwrap_or(Value::Text(s)),
        ColType::Float64 => s.parse().map(Value::Float).unwrap_or(Value::Text(s)),
        ColType::Date => parse_date(&s).map(Value::Date).unwrap_or(Value::Text(s)),
        ColType::Timestamp => parse_datetime(&s).map(Value::Timestamp).unwrap_or(Value::Text(s)),
        _ => Value::Text(s),
    }
}

/// Fields of a TabSeparated line (`\N` = NULL, backslash escapes).
fn split_tsv(line: &[u8]) -> Vec<Option<String>> {
    line.split(|b| *b == b'\t')
        .map(|f| {
            if f == b"\\N" {
                return None;
            }
            let mut out = Vec::with_capacity(f.len());
            let mut it = f.iter();
            while let Some(&b) = it.next() {
                if b != b'\\' {
                    out.push(b);
                    continue;
                }
                match it.next() {
                    Some(b'n') => out.push(b'\n'),
                    Some(b't') => out.push(b'\t'),
                    Some(b'r') => out.push(b'\r'),
                    Some(b'b') => out.push(8),
                    Some(b'f') => out.push(12),
                    Some(b'0') => out.push(0),
                    Some(&c) => out.push(c),
                    None => out.push(b'\\'),
                }
            }
            Some(String::from_utf8_lossy(&out).into_owned())
        })
        .collect()
}

fn object_kind(engine: &str) -> ObjectKind {
    match engine {
        "View" | "LiveView" | "WindowView" => ObjectKind::View,
        "MaterializedView" => ObjectKind::MaterializedView,
        e if [
            "MySQL",
            "PostgreSQL",
            "MongoDB",
            "Redis",
            "SQLite",
            "ODBC",
            "JDBC",
            "URL",
            "S3",
            "S3Queue",
            "AzureBlobStorage",
            "AzureQueue",
            "HDFS",
            "File",
            "Kafka",
            "RabbitMQ",
            "NATS",
            "DeltaLake",
            "Hudi",
            "ExternalDistributed",
        ]
        .contains(&e)
            || e.starts_with("Iceberg") =>
        {
            ObjectKind::ForeignTable
        }
        _ => ObjectKind::Table,
    }
}

fn opt_text(v: &Option<String>) -> Option<String> {
    v.clone().filter(|s| !s.trim().is_empty())
}

fn column_info(row: &[Option<String>]) -> ColumnInfo {
    let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let ty = get(1);
    let default_kind = get(2);
    let default_expr = get(3);
    let outer = ty.trim();
    let nullable = outer.strip_prefix("LowCardinality(").unwrap_or(outer).starts_with("Nullable(");
    ColumnInfo {
        name: get(0),
        nullable,
        data_type: ty,
        is_primary_key: get(5) == "1",
        default: match default_kind.as_str() {
            "" => None,
            "DEFAULT" => Some(default_expr),
            k => Some(format!("{k} {default_expr}")),
        },
        comment: opt_text(&row.get(4).cloned().flatten()),
    }
}

const COLUMNS: &str = "SELECT name, type, default_kind, default_expression, comment, is_in_primary_key FROM system.columns";

pub struct ChSession(Arc<Inner>);

#[async_trait]
impl Session for ChSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Clickhouse
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.0.rows("SELECT version()", false).await?;
        Ok(format!("ClickHouse {}", r.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default()))
    }

    async fn ping(&self) -> Result<()> {
        self.0.rows("SELECT 1", false).await.map(|_| ())
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
        let current = self.0.database.lock().unwrap().clone();
        let rows = self.0.rows("SELECT name FROM system.databases WHERE lower(name) != 'information_schema' ORDER BY name", false).await?;
        let mut out: Vec<SchemaInfo> = rows
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .map(|n| {
                let d = current.as_deref() == Some(n.as_str());
                SchemaInfo::new(n, d)
            })
            .collect();
        out.sort_by_key(|s| !s.is_default);
        Ok(out)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let sql = format!("SELECT name, engine, comment, total_rows FROM system.tables WHERE database = {} AND NOT is_temporary ORDER BY name", lit(schema));
        Ok(self
            .0
            .rows(&sql, false)
            .await?
            .into_iter()
            .map(|r| DbObject {
                schema: schema.to_string(),
                name: r.first().cloned().flatten().unwrap_or_default(),
                kind: object_kind(r.get(1).cloned().flatten().as_deref().unwrap_or("")),
                comment: opt_text(&r.get(2).cloned().flatten()),
                row_estimate: r.get(3).cloned().flatten().and_then(|n| n.parse().ok()),
            })
            .collect())
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let t = self
            .0
            .rows(
                &format!("SELECT engine, comment, total_rows, create_table_query FROM system.tables WHERE database = {} AND name = {}", lit(schema), lit(name)),
                false,
            )
            .await?;
        let Some(t) = t.into_iter().next() else {
            return Err(ConnectorError::query(format!("{schema}.{name} not found")));
        };
        let cols = self.0.rows(&format!("{COLUMNS} WHERE database = {} AND table = {} ORDER BY position", lit(schema), lit(name)), false).await?;
        Ok(ObjectDetail {
            object: DbObject {
                schema: schema.to_string(),
                name: name.to_string(),
                kind: object_kind(t.first().cloned().flatten().as_deref().unwrap_or("")),
                comment: opt_text(&t.get(1).cloned().flatten()),
                row_estimate: t.get(2).cloned().flatten().and_then(|n| n.parse().ok()),
            },
            columns: cols.iter().map(|r| column_info(r)).collect(),
            ddl: opt_text(&t.get(3).cloned().flatten()),
            foreign_keys: vec![],
        })
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let sql = format!(
            "SELECT name, type, default_kind, default_expression, comment, is_in_primary_key, table FROM system.columns WHERE database = {} ORDER BY table, position",
            lit(schema)
        );
        let mut out: Vec<TableColumns> = Vec::new();
        for r in self.0.rows(&sql, false).await? {
            let table = r.get(6).cloned().flatten().unwrap_or_default();
            let col = column_info(&r);
            match out.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => out.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        Ok(out)
    }

    /// Primary key, partition key, sorting key and data skipping indexes (MergeTree).
    async fn table_layout(&self, schema: &str, name: &str) -> Result<TableLayout> {
        let (s, n) = (lit(schema), lit(name));
        let t = self
            .0
            .rows(
                &format!(
                    "SELECT primary_key, partition_key, sorting_key, total_rows, engine, total_bytes FROM system.tables WHERE database = {s} AND name = {n}"
                ),
                false,
            )
            .await?;
        let Some(t) = t.into_iter().next() else { return Ok(TableLayout::default()) };
        let get = |i: usize| t.get(i).cloned().flatten().unwrap_or_default();
        let mut indexes = Vec::new();
        let pk = split_key_list(&get(0));
        if !pk.is_empty() {
            indexes.push(IndexInfo { name: "PRIMARY KEY".into(), columns: pk, unique: false, primary: true, method: None });
        }
        // Older servers have no data_skipping_indices table.
        if let Ok(rows) = self.0.rows(&format!("SELECT name, expr, type FROM system.data_skipping_indices WHERE database = {s} AND table = {n}"), false).await {
            for r in rows {
                let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
                indexes.push(IndexInfo {
                    name: g(0),
                    columns: split_key_list(&g(1)),
                    unique: false,
                    primary: false,
                    method: Some(g(2)).filter(|m| !m.is_empty()),
                });
            }
        }
        let engine = get(4);
        Ok(TableLayout {
            indexes,
            partition_by: split_key_list(&get(1)),
            partition_kind: None,
            cluster_by: split_key_list(&get(2)),
            requires_partition_filter: false,
            row_estimate: get(3).parse().ok(),
            size_bytes: get(5).parse().ok(),
            notes: if engine.is_empty() { vec![] } else { vec![format!("ENGINE = {engine}")] },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;

    /// A fake server: answers the sign-in check, then streams rows and fails
    /// the way ClickHouse 24.11+ does once the result has started: an
    /// exception block, then the connection closed without the last chunk.
    fn broken_stream_server(with_error: bool) -> u16 {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                // The whole request: headers, then Content-Length bytes of SQL.
                let mut req = Vec::new();
                let mut b = [0u8; 4096];
                loop {
                    let n = c.read(&mut b).unwrap();
                    req.extend_from_slice(&b[..n]);
                    let text = String::from_utf8_lossy(&req).to_lowercase();
                    let Some(end) = text.find("\r\n\r\n") else { continue };
                    let len = text.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse::<usize>().unwrap());
                    if n == 0 || req.len() >= end + 4 + len {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&req);
                if req.contains("currentDatabase()") {
                    let body = "currentDatabase()\nString\ndefault\n";
                    let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    continue;
                }
                let tag = "abcdefghijklmnop";
                let chunk = |c: &mut std::net::TcpStream, s: &str| write!(c, "{:x}\r\n{s}\r\n", s.len()).unwrap();
                write!(c, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nX-ClickHouse-Exception-Tag: {tag}\r\n\r\n").unwrap();
                chunk(&mut c, "x\nUInt8\n0\n0\n");
                let msg = "Code: 395. DB::Exception: Value passed to 'throwIf' function is non-zero: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) (version 25.8.1.1)\n";
                if with_error {
                    chunk(&mut c, &format!("\r\n__exception__\r\n{tag}\r\n{msg}{} {tag}\r\n__exception__\r\n", msg.len()));
                }
                c.flush().unwrap();
                // No terminating chunk: the response is broken on purpose.
            }
        });
        port
    }

    #[tokio::test]
    async fn reports_errors_sent_after_the_result_started() {
        use databrain_auth::InlineCredentialSource;
        for with_error in [true, false] {
            let port = broken_stream_server(with_error);
            let mut cfg = ConnectionConfig::new(ConnectorKind::Clickhouse, AuthMethod::Password { user: "default".into() });
            cfg.host = Some("127.0.0.1".into());
            cfg.port = Some(port);
            let creds = Arc::new(InlineCredentialSource::new(AuthMethod::Password { user: "default".into() }, Some(secrecy::SecretString::from("pw"))));
            let s = ClickhouseConnector.connect(&cfg, creds).await.unwrap();
            let e = s.execute("SELECT throwIf(number = 100)", ExecOptions::default()).await.unwrap().collect().await.unwrap_err();
            if with_error {
                assert!(e.message.contains("boom") && !e.message.contains("abcdefghijklmnop"), "{e:?}");
            } else {
                // Without the message: says what happened and where to find it.
                assert!(e.message.contains("after 2 rows") && e.message.contains("system.query_log (query_id = '"), "{e:?}");
            }
        }
    }

    #[test]
    fn endpoints() {
        let mut c = ConnectionConfig::new(ConnectorKind::Clickhouse, AuthMethod::Password { user: "u".into() });
        c.host = Some("ch.local".into());
        assert_eq!(endpoint(&c), ("http://ch.local:8123/".into(), false));
        c.port = Some(8443);
        assert_eq!(endpoint(&c).0, "https://ch.local:8443/");
        c.port = None;
        c.ssl_mode = SslMode::Require;
        assert_eq!(endpoint(&c), ("https://ch.local:8443/".into(), true));
        c.ssl_mode = SslMode::VerifyFull;
        assert_eq!(endpoint(&c), ("https://ch.local:8443/".into(), false));
        c.host = Some("https://x.clickhouse.cloud/".into());
        c.ssl_mode = SslMode::Disable;
        assert_eq!(endpoint(&c).0, "https://x.clickhouse.cloud:8443/");
        c.host = Some("::1".into());
        assert_eq!(endpoint(&c).0, "http://[::1]:8123/");
    }

    #[test]
    fn tsv_and_types() {
        assert_eq!(split_tsv(b"a\\tb\t\\N\tx\\\\y\t\\'q"), vec![Some("a\tb".into()), None, Some("x\\y".into()), Some("'q".into())]);
        assert_eq!(col_type("Nullable(UInt64)"), ColType::UInt64);
        assert_eq!(col_type("LowCardinality(Nullable(String))"), ColType::Utf8);
        assert_eq!(col_type("DateTime64(3, 'UTC')"), ColType::Timestamp);
        assert_eq!(col_type("Decimal(18, 4)"), ColType::Utf8);
        assert_eq!(cell(ColType::UInt64, Some("18446744073709551615".into())), Value::UInt(u64::MAX));
        assert_eq!(cell(ColType::Int64, None), Value::Null);
        let c = column_info(&[
            Some("a".into()),
            Some("Nullable(Int32)".into()),
            Some("MATERIALIZED".into()),
            Some("b + 1".into()),
            Some("".into()),
            Some("0".into()),
        ]);
        assert!(c.nullable);
        assert_eq!(c.default.as_deref(), Some("MATERIALIZED b + 1"));
        assert!(!column_info(&[Some("a".into()), Some("Int32".into())]).nullable);
    }

    #[test]
    fn errors() {
        let e = server_error(Some(60), 404, "Code: 60. DB::Exception: Table default.x does not exist. (UNKNOWN_TABLE) (version 24.3.2.23 (official build))\n");
        assert_eq!((e.kind, e.message.as_str(), e.code.as_deref()), (ErrorKind::Query, "Table default.x does not exist.", Some("60 (UNKNOWN_TABLE)")));
        let e = server_error(
            None,
            400,
            "Code: 62. DB::Exception: Syntax error: failed at position 8 ('fro'): fro x. Expected one of: ... (SYNTAX_ERROR) (version 24.3)",
        );
        assert_eq!((e.position, e.code.as_deref()), (Some(8), Some("62 (SYNTAX_ERROR)")));
        assert_eq!(server_error(Some(516), 403, "Code: 516. DB::Exception: default: Authentication failed. (AUTHENTICATION_FAILED)").kind, ErrorKind::Auth);
        assert_eq!(server_error(Some(394), 500, "Code: 394. DB::Exception: Query was cancelled. (QUERY_WAS_CANCELLED)").kind, ErrorKind::Cancelled);
    }

    #[test]
    fn use_statements() {
        assert_eq!(use_target("USE analytics;").as_deref(), Some("analytics"));
        assert_eq!(use_target("use `my db`").as_deref(), Some("my db"));
        assert_eq!(use_target("select 1"), None);
        assert_eq!(lit("a'b\\"), "'a\\'b\\\\'");
    }

    /// Live test: set DATABRAIN_CLICKHOUSE_HOST (+ _PORT, _USER, _PASSWORD).
    #[ignore = "live: needs DATABRAIN_CLICKHOUSE_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(host) = std::env::var("DATABRAIN_CLICKHOUSE_HOST") else {
            panic!("live test: DATABRAIN_CLICKHOUSE_HOST not set (run with --ignored)");
        };
        use databrain_auth::InlineCredentialSource;
        let user = std::env::var("DATABRAIN_CLICKHOUSE_USER").unwrap_or_else(|_| "default".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Clickhouse, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.port = std::env::var("DATABRAIN_CLICKHOUSE_PORT").ok().and_then(|p| p.parse().ok());
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user },
            std::env::var("DATABRAIN_CLICKHOUSE_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        let s = ClickhouseConnector.connect(&cfg, creds).await.unwrap();
        assert!(s.server_version().await.unwrap().starts_with("ClickHouse "));
        let s = &s;
        let run = |sql: &'static str| async move { s.execute(sql, ExecOptions::default()).await.unwrap().collect().await };
        run("CREATE DATABASE IF NOT EXISTS databrain_t").await.unwrap();
        run("USE databrain_t").await.unwrap();
        run("CREATE OR REPLACE TABLE t (id UInt64, name Nullable(String), d Date, ts DateTime64(3), amt Decimal(18, 4)) ENGINE = MergeTree PARTITION BY toYYYYMM(d) ORDER BY id")
            .await
            .unwrap();
        let ins =
            run("INSERT INTO t VALUES (1, 'a\\tb', '2024-01-02', '2024-01-02 03:04:05.678', 1.5), (18446744073709551615, NULL, '2024-02-01', now64(), 2)")
                .await
                .unwrap();
        assert_eq!(ins.summary.rows_affected, Some(2));
        let r = run("SELECT * FROM t ORDER BY id").await.unwrap();
        assert_eq!(r.num_rows(), 2);
        let t = databrain_connector_core::query_text(s.as_ref(), "SELECT id, name, d, amt FROM t ORDER BY id").await.unwrap();
        assert_eq!((t[0][1].as_deref(), t[0][2].as_deref(), t[0][3].as_deref()), (Some("a\tb"), Some("2024-01-02"), Some("1.5")));
        assert_eq!((t[1][0].as_deref(), t[1][1].as_deref()), (Some("18446744073709551615"), None));
        // An error after the first rows were sent (HTTP 200 already).
        let e = run("SELECT throwIf(number = 50000, 'boom') FROM numbers(100000) SETTINGS max_block_size = 100").await.unwrap_err();
        // ClickHouse 25.8 may break the response off without the message.
        assert!(e.message.contains("boom") || e.message.contains("system.query_log"), "{e:?}");
        // Session state: a temporary table survives to the next statement.
        run("CREATE TEMPORARY TABLE tmp (x Int32)").await.unwrap();
        run("INSERT INTO tmp VALUES (7)").await.unwrap();
        assert_eq!(run("SELECT x FROM tmp").await.unwrap().num_rows(), 1);
        let e = run("SELECT nope FROM t").await.unwrap_err();
        assert!(e.message.contains("nope"), "{e:?}");
        let objs = s.list_objects("databrain_t").await.unwrap();
        assert_eq!(objs[0].name, "t");
        let d = s.describe("databrain_t", "t").await.unwrap();
        assert!(d.columns[0].is_primary_key && d.columns[1].nullable && d.ddl.unwrap().contains("MergeTree"));
        let l = s.table_layout("databrain_t", "t").await.unwrap();
        assert_eq!((l.indexes[0].columns.clone(), l.partition_by.clone()), (vec!["id".to_string()], vec!["toYYYYMM(d)".to_string()]));
        assert_eq!(l.row_estimate, Some(2), "{l:?}");
        assert!(l.size_bytes.is_some_and(|b| b > 0), "{l:?}");
        assert_eq!(s.schema_columns("databrain_t").await.unwrap()[0].columns.len(), 5);
        // A read-only connection: the server refuses writes.
        let mut ro = cfg.clone();
        ro.read_only = true;
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user: ro.auth.user().unwrap_or("default").to_string() },
            std::env::var("DATABRAIN_CLICKHOUSE_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        let r = ClickhouseConnector.connect(&ro, creds).await.unwrap();
        assert!(r.execute("INSERT INTO databrain_t.t (id) VALUES (3)", ExecOptions::default()).await.unwrap().collect().await.is_err());
        assert_eq!(r.execute("SELECT count() FROM databrain_t.t", ExecOptions::default()).await.unwrap().collect().await.unwrap().num_rows(), 1);
        // Cancel a long query.
        let cancel = databrain_connector_core::CancellationToken::new();
        let stream = s
            .execute("SELECT sleepEachRow(1) FROM numbers(30) SETTINGS max_block_size = 1", ExecOptions { cancel: cancel.clone(), ..Default::default() })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel();
        assert_eq!(stream.collect().await.unwrap_err().kind, ErrorKind::Cancelled);
        run("DROP DATABASE databrain_t").await.unwrap();
    }
}
