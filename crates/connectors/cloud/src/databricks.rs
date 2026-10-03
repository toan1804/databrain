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
            host,
            warehouse: warehouse_id(cfg)?,
            creds,
            http: http(),
            catalog: Mutex::new(cfg.opt("catalog").map(str::to_string)),
            schema: Mutex::new(cfg.opt("schema").map(str::to_string)),
        }));
        // Validate credentials + warehouse up front.
        s.0.run_small("SELECT 1").await?;
        Ok(Box::new(s))
    }
}

pub struct DbxSession(Arc<Inner>);

struct Inner {
    host: String,
    base: String,
    warehouse: String,
    creds: Arc<dyn CredentialSource>,
    http: reqwest::Client,
    /// Session context emulated client-side (the API is stateless): `USE` updates these.
    catalog: Mutex<Option<String>>,
    schema: Mutex<Option<String>>,
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
        let mut body = json!({
            "statement": sql,
            "warehouse_id": self.warehouse,
            "wait_timeout": "10s",
            "on_wait_timeout": "CONTINUE",
            "disposition": "INLINE",
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
        let resp = self.submit(sql, Some(100_000)).await?;
        let resp = self.wait(resp, &databrain_connector_core::CancellationToken::new()).await?;
        let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
        let mut chunk = resp.get("result").cloned();
        while let Some(c) = chunk {
            if let Some(arr) = c["data_array"].as_array() {
                rows.extend(arr.iter().map(|r| r.as_array().cloned().unwrap_or_default()));
            }
            chunk = match c.get("next_chunk_internal_link").and_then(|l| l.as_str()) {
                Some(link) => Some(self.request(reqwest::Method::GET, format!("https://{}{link}", self.host), None).await?),
                None => None,
            };
        }
        Ok(rows)
    }

    async fn stream(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
        // Emulate USE for subsequent statements (the API is stateless).
        let kw = databrain_connector_core::sql::leading_keyword(sql);
        let resp = self.submit(sql, None).await?;
        let resp = self.wait(resp, &opts.cancel).await?;
        if kw == "USE" {
            self.apply_use(sql).await;
        }
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
        let Some(mut sink) = RowSink::start(&columns, opts.batch_size, tx).await else { return Ok(()) };
        let mut chunk = resp.get("result").cloned();
        'outer: while let Some(c) = chunk {
            if let Some(arr) = c["data_array"].as_array() {
                for r in arr {
                    let row = r.as_array().cloned().unwrap_or_default();
                    let vals = sink.types.clone().into_iter().enumerate().map(|(i, t)| json_cell(t, row.get(i).unwrap_or(&serde_json::Value::Null)));
                    if !sink.push(vals).await? {
                        break 'outer;
                    }
                }
            }
            if opts.cancel.is_cancelled() {
                return Err(ConnectorError::cancelled());
            }
            chunk = match c.get("next_chunk_internal_link").and_then(|l| l.as_str()) {
                Some(link) => Some(self.request(reqwest::Method::GET, format!("https://{}{link}", self.host), None).await?),
                None => None,
            };
        }
        sink.finish().await?;
        tx.send(StreamEvent::Done(ExecSummary::default())).await;
        Ok(())
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
        Ok(rows
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
            .collect())
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
            .find(|o| o.name == name)
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
}
