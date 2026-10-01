//! Snowflake via the driver REST protocol (the same endpoints the official
//! connectors use): `session/v1/login-request` + `queries/v1/query-request`.
//!
//! Auth: password (+ MFA push/passcode), key pair (`SNOWFLAKE_JWT`),
//! programmatic access token, OAuth (Snowflake OAuth security integration or
//! External OAuth via your IdP, browser + PKCE) and `externalbrowser` SAML
//! SSO. Results are requested as JSON (`JSON` result format) and large
//! results are downloaded chunk by chunk.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use databrain_auth::{
    AuthContext, AuthMethodKind, ClientAuthStyle, Credential, CredentialSource, Interaction, OAuthConfig, now_secs,
};
use databrain_connector_core::value::Column;
use databrain_connector_core::{
    Capabilities, ColType, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind,
    DbObject, ErrorKind, ExecOptions, ExecSummary, FieldSpec, ForeignKey, ObjectDetail, ObjectKind, QueryStream, Result,
    SchemaInfo, Session, StreamEvent, StreamSender, TableColumns, Value, quote_ident, quote_literal,
};
use secrecy::ExposeSecret;
use serde_json::json;
use tokio::sync::Mutex;

use crate::common::{Backoff, RowSink, http, net_err, truncate};

#[derive(Debug, Default)]
pub struct SnowflakeConnector;

impl SnowflakeConnector {
    pub fn new() -> Self {
        Self
    }
}

/// Account locator/org-account as used in the URL (`xy12345.eu-west-1`, `myorg-myacct`).
fn account(cfg: &ConnectionConfig) -> Result<String> {
    let a = cfg
        .opt("account")
        .map(str::to_string)
        .or_else(|| cfg.host.as_deref().map(|h| h.trim_end_matches(".snowflakecomputing.com").to_string()))
        .map(|a| a.trim().trim_start_matches("https://").trim_end_matches('/').trim_end_matches(".snowflakecomputing.com").to_string())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| ConnectorError::config("Snowflake account identifier is required (e.g. myorg-myaccount)"))?;
    Ok(a)
}

fn host(cfg: &ConnectionConfig, account: &str) -> String {
    match cfg.host.as_deref().filter(|h| h.contains('.')) {
        Some(h) => h.trim().trim_start_matches("https://").trim_end_matches('/').to_string(),
        None => format!("{account}.snowflakecomputing.com"),
    }
}

/// Account name for JWT claims: upper-cased, without region/cloud segments.
fn jwt_account(account: &str) -> String {
    let a = account.split('.').next().unwrap_or(account);
    a.to_ascii_uppercase()
}

#[async_trait]
impl Connector for SnowflakeConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Snowflake,
            display_name: "Snowflake",
            default_port: None,
            uses_file: false,
            auth_methods: vec![
                AuthMethodKind::ExternalBrowser,
                AuthMethodKind::OauthBrowser,
                AuthMethodKind::ApiToken,
                AuthMethodKind::KeyPair,
                AuthMethodKind::Password,
            ],
            capabilities: Capabilities { transactions: true, cancel: true, schemas: true, read_only_sessions: false, ssh: false },
            fields: vec![
                FieldSpec::new("account", "Account identifier").required().placeholder("myorg-myaccount or xy12345.eu-west-1"),
                FieldSpec::new("warehouse", "Warehouse").placeholder("COMPUTE_WH"),
                FieldSpec::new("database", "Database").placeholder("(optional)"),
                FieldSpec::new("schema", "Schema").placeholder("PUBLIC"),
                FieldSpec::new("role", "Role").placeholder("(optional)"),
                FieldSpec::new("authenticator", "Okta URL").placeholder("(optional) https://myorg.okta.com for native SSO")
                    .help("Leave empty for external browser SSO"),
            ],
            note: Some("Browser SSO uses your identity provider via Snowflake. OAuth needs a security integration client ID."),
        }
    }

    fn auth_context(&self, cfg: &ConnectionConfig) -> AuthContext {
        let acct = account(cfg).unwrap_or_default();
        let h = host(cfg, &acct);
        let p = cfg.auth.oauth_params().cloned().unwrap_or_default();
        let mut scopes: Vec<String> = p.scopes.as_deref().map(|s| s.split_whitespace().map(str::to_string).collect()).unwrap_or_default();
        if scopes.is_empty() {
            if let Some(r) = cfg.opt("role") {
                scopes.push(format!("session:role:{r}"));
            }
            scopes.push("refresh_token".into());
        }
        AuthContext {
            oauth: Some(OAuthConfig {
                provider: "Snowflake".into(),
                authorize_url: p.authorize_url.clone().unwrap_or_else(|| format!("https://{h}/oauth/authorize")),
                token_url: p.token_url.clone().unwrap_or_else(|| format!("https://{h}/oauth/token-request")),
                device_url: None,
                revoke_url: None,
                client_id: p.client_id.unwrap_or_default(),
                client_secret: None,
                scopes,
                redirect_host: "127.0.0.1".into(),
                redirect_port: p.redirect_port,
                redirect_path: "/".into(),
                extra_authorize_params: vec![],
                client_auth: ClientAuthStyle::Basic,
            }),
            cli: None,
            google_scopes: vec![],
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let acct = account(cfg)?;
        let base = format!("https://{}", host(cfg, &acct));
        let http = http();
        let token = login(&http, &base, &acct, cfg, creds.as_ref()).await?;
        let inner = Arc::new(Inner { base, http, token: Mutex::new(token), request_seq: std::sync::atomic::AtomicU64::new(1) });
        Ok(Box::new(SfSession(inner)))
    }
}

struct Tokens {
    session: String,
    master: String,
}

fn client_env() -> serde_json::Value {
    json!({
        "APPLICATION": "DataBrain",
        "OS": std::env::consts::OS,
        "OS_VERSION": "",
        "OCSP_MODE": "FAIL_OPEN",
    })
}

async fn login(http: &reqwest::Client, base: &str, acct: &str, cfg: &ConnectionConfig, creds: &dyn CredentialSource) -> Result<Tokens> {
    let mut data = json!({
        "CLIENT_APP_ID": "DataBrain",
        "CLIENT_APP_VERSION": env!("CARGO_PKG_VERSION"),
        "ACCOUNT_NAME": jwt_account(acct),
        "CLIENT_ENVIRONMENT": client_env(),
        "SESSION_PARAMETERS": {
            "CLIENT_SESSION_KEEP_ALIVE": true,
            "QUERY_RESULT_FORMAT": "JSON",
            "TIMESTAMP_OUTPUT_FORMAT": "YYYY-MM-DD HH24:MI:SS.FF6 TZHTZM",
        },
    });
    match creds.get().await? {
        Credential::Password { user, password } => {
            data["LOGIN_NAME"] = user.into();
            data["PASSWORD"] = password.expose_secret().into();
            if let Some(code) = cfg.opt("passcode") {
                data["EXT_AUTHN_DUO_METHOD"] = "passcode".into();
                data["PASSCODE"] = code.into();
            }
        }
        Credential::Bearer { token, user } => {
            // PAT and OAuth tokens both use the OAUTH authenticator.
            data["AUTHENTICATOR"] = if cfg.auth.kind() == AuthMethodKind::ApiToken { "PROGRAMMATIC_ACCESS_TOKEN" } else { "OAUTH" }.into();
            data["TOKEN"] = token.expose_secret().into();
            if let Some(u) = user {
                data["LOGIN_NAME"] = u.into();
            }
        }
        Credential::KeyPair { user, private_key_pem, passphrase } => {
            let key = databrain_auth::jwt::parse_private_key(private_key_pem.expose_secret(), passphrase.as_ref().map(|p| p.expose_secret()))
                .map_err(|e| ConnectorError::new(ErrorKind::Auth, e.to_string()))?;
            let fp = databrain_auth::jwt::public_key_fingerprint(&key).map_err(|e| ConnectorError::new(ErrorKind::Auth, e.to_string()))?;
            let qual = format!("{}.{}", jwt_account(acct), user.to_ascii_uppercase());
            let now = now_secs();
            let jwt = databrain_auth::jwt::sign_rs256(
                &key,
                &json!({"alg": "RS256", "typ": "JWT"}),
                &json!({"iss": format!("{qual}.{fp}"), "sub": qual, "iat": now, "exp": now + 3540}),
            )
            .map_err(|e| ConnectorError::new(ErrorKind::Auth, e.to_string()))?;
            data["AUTHENTICATOR"] = "SNOWFLAKE_JWT".into();
            data["LOGIN_NAME"] = user.into();
            data["TOKEN"] = jwt.into();
        }
        Credential::ExternalBrowser { user, interaction } => {
            let (token, proof) = external_browser(http, base, acct, &user, interaction.as_ref()).await?;
            data["AUTHENTICATOR"] = "EXTERNALBROWSER".into();
            data["LOGIN_NAME"] = user.into();
            data["TOKEN"] = token.into();
            data["PROOF_KEY"] = proof.into();
        }
        Credential::None => return Err(ConnectorError::config("Snowflake requires authentication")),
    }
    let mut q: Vec<(&str, String)> = vec![("request_id", uuid::Uuid::new_v4().to_string())];
    for (k, param) in [("warehouse", "warehouse"), ("databaseName", "database"), ("schemaName", "schema"), ("roleName", "role")] {
        let v = if param == "database" { cfg.opt("database").or(cfg.database.as_deref().filter(|d| !d.is_empty())) } else { cfg.opt(param) };
        if let Some(v) = v {
            q.push((k, v.to_string()));
        }
    }
    let resp = http
        .post(format!("{base}/session/v1/login-request"))
        .query(&q)
        .header("Accept", "application/json")
        .json(&json!({ "data": data }))
        .send()
        .await
        .map_err(net_err)?;
    let v: serde_json::Value = resp.json().await.map_err(net_err)?;
    if !v["success"].as_bool().unwrap_or(false) {
        let code = v["code"].as_str().unwrap_or_default();
        let msg = v["message"].as_str().unwrap_or("login failed");
        let kind = if code == "390100" || code == "390144" || code == "394304" { ErrorKind::Auth } else { ErrorKind::Connection };
        if matches!(code, "390303" | "390318") {
            creds.invalidate().await;
        }
        return Err(ConnectorError::new(kind, msg.to_string()).with_code(code.to_string()));
    }
    Ok(Tokens {
        session: v.pointer("/data/token").and_then(|t| t.as_str()).unwrap_or_default().to_string(),
        master: v.pointer("/data/masterToken").and_then(|t| t.as_str()).unwrap_or_default().to_string(),
    })
}

/// `externalbrowser` SAML SSO: ask Snowflake for the IdP URL, open it, and
/// receive the SAML token on a loopback listener.
async fn external_browser(http: &reqwest::Client, base: &str, acct: &str, user: &str, ui: &dyn Interaction) -> Result<(String, String)> {
    if !ui.interactive() {
        return Err(ConnectorError::new(ErrorKind::Auth, "sign-in required"));
    }
    let lb = databrain_auth::loopback::Loopback::bind("localhost", 0, "/").await?;
    let port = lb.port();
    let resp = http
        .post(format!("{base}/session/authenticator-request"))
        .json(&json!({ "data": {
            "ACCOUNT_NAME": jwt_account(acct),
            "LOGIN_NAME": user,
            "AUTHENTICATOR": "EXTERNALBROWSER",
            "BROWSER_MODE_REDIRECT_PORT": port.to_string(),
            "CLIENT_APP_ID": "DataBrain",
            "CLIENT_APP_VERSION": env!("CARGO_PKG_VERSION"),
        }}))
        .send()
        .await
        .map_err(net_err)?;
    let v: serde_json::Value = resp.json().await.map_err(net_err)?;
    if !v["success"].as_bool().unwrap_or(false) {
        return Err(ConnectorError::new(ErrorKind::Auth, v["message"].as_str().unwrap_or("SSO request failed").to_string()));
    }
    let sso = v.pointer("/data/ssoUrl").and_then(|s| s.as_str()).unwrap_or_default().to_string();
    let proof = v.pointer("/data/proofKey").and_then(|s| s.as_str()).unwrap_or_default().to_string();
    ui.open_url(&sso).await?;
    let params = lb.wait(None, &ui.cancel_token(), Duration::from_secs(300)).await;
    ui.finished().await;
    let params = params?;
    let token = params
        .get("token")
        .cloned()
        .ok_or_else(|| ConnectorError::new(ErrorKind::Auth, "the SSO redirect did not include a token"))?;
    Ok((token, proof))
}

pub struct SfSession(Arc<Inner>);

struct Inner {
    base: String,
    http: reqwest::Client,
    token: Mutex<Tokens>,
    request_seq: std::sync::atomic::AtomicU64,
}

fn col_type(rt: &serde_json::Value) -> (ColType, String) {
    let t = rt["type"].as_str().unwrap_or("text").to_ascii_lowercase();
    let scale = rt["scale"].as_i64().unwrap_or(0);
    let precision = rt["precision"].as_i64().unwrap_or(38);
    let ct = match t.as_str() {
        "fixed" if scale == 0 && precision <= 18 => ColType::Int64,
        "fixed" => ColType::Utf8,
        "real" => ColType::Float64,
        "boolean" => ColType::Bool,
        "date" => ColType::Date,
        "time" => ColType::Time,
        "timestamp_ntz" => ColType::Timestamp,
        "timestamp_ltz" | "timestamp_tz" => ColType::TimestampTz,
        "binary" => ColType::Binary,
        _ => ColType::Utf8,
    };
    let db = match t.as_str() {
        "fixed" if scale == 0 => format!("number({precision})"),
        "fixed" => format!("number({precision},{scale})"),
        "text" => "varchar".into(),
        other => other.to_string(),
    };
    (ct, db)
}

/// Snowflake JSON rowsets encode dates/times as epoch numbers in strings.
fn sf_cell(t: ColType, v: &serde_json::Value) -> Value {
    let Some(s) = v.as_str() else {
        return if v.is_null() { Value::Null } else { Value::Text(v.to_string()) };
    };
    let secs_to_micros = |s: &str| -> Option<i64> {
        let (i, f) = s.split_once('.').unwrap_or((s, ""));
        let whole: i64 = i.parse().ok()?;
        let mut frac = f.chars().take(6).collect::<String>();
        while frac.len() < 6 {
            frac.push('0');
        }
        let fr: i64 = frac.parse().unwrap_or(0);
        Some(whole * 1_000_000 + if whole < 0 || i.starts_with('-') { -fr } else { fr })
    };
    match t {
        ColType::Int64 => s.parse().map(Value::Int).unwrap_or_else(|_| Value::Text(s.into())),
        ColType::Float64 => s.parse().map(Value::Float).unwrap_or_else(|_| Value::Text(s.into())),
        ColType::Bool => Value::Bool(s == "1" || s.eq_ignore_ascii_case("true")),
        ColType::Date => s.parse::<i32>().map(Value::Date).unwrap_or_else(|_| Value::Text(s.into())),
        ColType::Time => secs_to_micros(s).map(Value::Time).unwrap_or_else(|| Value::Text(s.into())),
        ColType::Timestamp => secs_to_micros(s).map(Value::Timestamp).unwrap_or_else(|| Value::Text(s.into())),
        ColType::TimestampTz => {
            // timestamp_tz: "<epoch.frac> <offset minutes + 1440>"
            let epoch = s.split_whitespace().next().unwrap_or(s);
            secs_to_micros(epoch).map(Value::TimestampTz).unwrap_or_else(|| Value::Text(s.into()))
        }
        ColType::Binary => crate::common::decode_hex(s).map(Value::Bytes).unwrap_or_else(|| Value::Text(s.into())),
        _ => Value::Text(s.to_string()),
    }
}

impl Inner {
    async fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let tok = self.token.lock().await.session.clone();
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .query(&[("requestId", uuid::Uuid::new_v4().to_string())])
            .header("Accept", "application/snowflake")
            .header("Authorization", format!("Snowflake Token=\"{tok}\""))
            .json(&body)
            .send()
            .await
            .map_err(net_err)?;
        let status = resp.status();
        let text = resp.text().await.map_err(net_err)?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| ConnectorError::connection(format!("Snowflake ({status}): {}", truncate(&text, 300))))?;
        // Session expired: renew with the master token and retry once.
        if v["code"].as_str() == Some("390112") {
            self.renew().await?;
            return Box::pin(self.post(path, body)).await;
        }
        Ok(v)
    }

    async fn get(&self, path: &str) -> Result<serde_json::Value> {
        let tok = self.token.lock().await.session.clone();
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .header("Accept", "application/snowflake")
            .header("Authorization", format!("Snowflake Token=\"{tok}\""))
            .send()
            .await
            .map_err(net_err)?;
        resp.json().await.map_err(net_err)
    }

    async fn renew(&self) -> Result<()> {
        let mut t = self.token.lock().await;
        let resp = self
            .http
            .post(format!("{}/session/token-request", self.base))
            .query(&[("requestId", uuid::Uuid::new_v4().to_string())])
            .header("Accept", "application/snowflake")
            .header("Authorization", format!("Snowflake Token=\"{}\"", t.master))
            .json(&json!({ "oldSessionToken": t.session, "requestType": "RENEW" }))
            .send()
            .await
            .map_err(net_err)?;
        let v: serde_json::Value = resp.json().await.map_err(net_err)?;
        if !v["success"].as_bool().unwrap_or(false) {
            return Err(ConnectorError::new(ErrorKind::Connection, "Snowflake session expired; reconnect"));
        }
        t.session = v.pointer("/data/sessionToken").and_then(|s| s.as_str()).unwrap_or_default().to_string();
        if let Some(m) = v.pointer("/data/masterToken").and_then(|s| s.as_str()) {
            t.master = m.to_string();
        }
        Ok(())
    }

    fn check(v: &serde_json::Value) -> Result<()> {
        if v["success"].as_bool().unwrap_or(false) {
            return Ok(());
        }
        let msg = v["message"].as_str().unwrap_or("query failed").to_string();
        let code = v["code"].as_str().unwrap_or_default().to_string();
        if code == "000604" {
            return Err(ConnectorError::cancelled());
        }
        let pos = v.pointer("/data/pos").and_then(|p| p.as_i64()).filter(|p| *p >= 0).map(|p| p as u32 + 1);
        Err(ConnectorError::query(msg).with_code(code).with_position(pos))
    }

    /// Submit and wait; returns the completed `data` object.
    async fn query(&self, sql: &str, request_id: &str, cancel: &databrain_connector_core::CancellationToken) -> Result<serde_json::Value> {
        let seq = self.request_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let body = json!({ "sqlText": sql, "asyncExec": false, "sequenceId": seq, "querySubmissionTime": now_secs() * 1000 });
        let path = format!("/queries/v1/query-request?requestId={request_id}");
        let submit = self.post(&path, body);
        tokio::pin!(submit);
        let mut v = tokio::select! {
            r = &mut submit => r?,
            _ = cancel.cancelled() => {
                self.abort(sql, request_id).await;
                return Err(ConnectorError::cancelled());
            }
        };
        // Long-running queries return 333334 ("in progress") with a result URL.
        let mut backoff = Backoff::new();
        while matches!(v["code"].as_str(), Some("333333") | Some("333334")) {
            let url = v.pointer("/data/getResultUrl").and_then(|u| u.as_str()).unwrap_or_default().to_string();
            tokio::select! {
                _ = backoff.wait() => {}
                _ = cancel.cancelled() => {
                    self.abort(sql, request_id).await;
                    return Err(ConnectorError::cancelled());
                }
            }
            v = self.get(&url).await?;
        }
        Self::check(&v)?;
        Ok(v["data"].clone())
    }

    async fn abort(&self, sql: &str, request_id: &str) {
        let _ = self.post("/queries/v1/abort-request", json!({ "sqlText": sql, "requestId": request_id })).await;
    }

    async fn run_small(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let rid = uuid::Uuid::new_v4().to_string();
        let data = self.query(sql, &rid, &databrain_connector_core::CancellationToken::new()).await?;
        let mut rows: Vec<Vec<Option<String>>> = data["rowset"]
            .as_array()
            .map(|a| a.iter().map(|r| r.as_array().map(|c| c.iter().map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()).collect())
            .unwrap_or_default();
        for r in self.chunks(&data).await? {
            rows.push(r.into_iter().map(|v| v.as_str().map(str::to_string)).collect());
        }
        Ok(rows)
    }

    /// Download all additional result chunks (JSON arrays without brackets, gzip).
    async fn chunks(&self, data: &serde_json::Value) -> Result<Vec<Vec<serde_json::Value>>> {
        let mut out = Vec::new();
        for c in data["chunks"].as_array().cloned().unwrap_or_default() {
            out.extend(self.chunk(data, &c).await?);
        }
        Ok(out)
    }

    async fn chunk(&self, data: &serde_json::Value, c: &serde_json::Value) -> Result<Vec<Vec<serde_json::Value>>> {
        let url = c["url"].as_str().unwrap_or_default();
        let mut req = self.http.get(url);
        if let Some(h) = data["chunkHeaders"].as_object() {
            for (k, v) in h {
                if let Some(v) = v.as_str() {
                    req = req.header(k, v);
                }
            }
        } else if let Some(key) = data["qrmk"].as_str() {
            req = req
                .header("x-amz-server-side-encryption-customer-algorithm", "AES256")
                .header("x-amz-server-side-encryption-customer-key", key);
        }
        let bytes = req.send().await.map_err(net_err)?.bytes().await.map_err(net_err)?;
        let text = if bytes.starts_with(&[0x1f, 0x8b]) {
            let mut s = String::new();
            flate2::read::GzDecoder::new(&bytes[..])
                .read_to_string(&mut s)
                .map_err(|e| ConnectorError::internal(format!("chunk decode: {e}")))?;
            s
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let rows: Vec<Vec<serde_json::Value>> = serde_json::from_str(&format!("[{text}]"))
            .map_err(|e| ConnectorError::internal(format!("chunk parse: {e}")))?;
        Ok(rows)
    }

    async fn stream(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
        let rid = uuid::Uuid::new_v4().to_string();
        let data = self.query(sql, &rid, &opts.cancel).await?;
        let rowtype = data["rowtype"].as_array().cloned().unwrap_or_default();
        let stmt_type = data["statementTypeId"].as_i64().unwrap_or(0);
        // DML (0x3000..0x4000) reports affected rows in the single result row.
        if (0x3000..0x4000).contains(&stmt_type) {
            let n: u64 = data["rowset"]
                .get(0)
                .and_then(|r| r.as_array())
                .map(|r| r.iter().filter_map(|v| v.as_str()?.parse::<u64>().ok()).sum())
                .unwrap_or(0);
            tx.send(StreamEvent::Done(ExecSummary { rows_affected: Some(n) })).await;
            return Ok(());
        }
        if rowtype.is_empty() {
            tx.send(StreamEvent::Done(ExecSummary::default())).await;
            return Ok(());
        }
        let types: Vec<(ColType, String)> = rowtype.iter().map(col_type).collect();
        let cols: Vec<Column> = rowtype
            .iter()
            .zip(&types)
            .map(|(r, (t, db))| Column::new(r["name"].as_str().unwrap_or(""), *t, db.clone()))
            .collect();
        let Some(mut sink) = RowSink::start(&cols, opts.batch_size, tx).await else { return Ok(()) };
        let first: Vec<Vec<serde_json::Value>> = data["rowset"]
            .as_array()
            .map(|a| a.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect())
            .unwrap_or_default();
        let mut pending = vec![first];
        let chunks = data["chunks"].as_array().cloned().unwrap_or_default();
        let mut next_chunk = 0usize;
        'outer: loop {
            let Some(rows) = pending.pop() else {
                if next_chunk >= chunks.len() {
                    break;
                }
                if opts.cancel.is_cancelled() {
                    return Err(ConnectorError::cancelled());
                }
                pending.push(self.chunk(&data, &chunks[next_chunk]).await?);
                next_chunk += 1;
                continue;
            };
            for r in rows {
                let vals: Vec<Value> = types.iter().enumerate().map(|(i, (t, _))| sf_cell(*t, r.get(i).unwrap_or(&serde_json::Value::Null))).collect();
                if !sink.push(vals).await? {
                    break 'outer;
                }
            }
        }
        sink.finish().await?;
        tx.send(StreamEvent::Done(ExecSummary::default())).await;
        Ok(())
    }
}

#[async_trait]
impl Session for SfSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Snowflake
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.0.run_small("select current_version(), current_account(), current_warehouse(), current_role()").await?;
        let row = r.into_iter().next().unwrap_or_default();
        let g = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
        Ok(format!("Snowflake {} — account {}, warehouse {}, role {}", g(0), g(1), g(2), g(3)))
    }

    async fn ping(&self) -> Result<()> {
        self.0.run_small("select 1").await.map(|_| ())
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
        let cur = self.0.run_small("select current_database(), current_schema()").await?;
        let (cur_db, cur_sch) = cur
            .first()
            .map(|r| (r.first().cloned().flatten(), r.get(1).cloned().flatten()))
            .unwrap_or((None, None));
        let dbs: Vec<String> = match &cur_db {
            Some(d) => vec![d.clone()],
            None => self
                .0
                .run_small("show terse databases")
                .await?
                .into_iter()
                .filter_map(|r| r.get(1).cloned().flatten())
                .collect(),
        };
        let mut out = Vec::new();
        for db in dbs {
            let rows = self.0.run_small(&format!("show terse schemas in database {}", quote_ident(ConnectorKind::Snowflake, &db))).await?;
            for r in rows {
                let Some(name) = r.get(1).cloned().flatten() else { continue };
                if name == "INFORMATION_SCHEMA" {
                    continue;
                }
                out.push(SchemaInfo::in_catalog(db.clone(), &name, cur_db.as_deref() == Some(db.as_str()) && cur_sch.as_deref() == Some(name.as_str())));
            }
        }
        out.sort_by_key(|s| !s.is_default);
        Ok(out)
    }

    async fn search_objects(&self, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let term = databrain_connector_core::search_sql_term(query);
        let cur = self.0.run_small("select current_database()").await?;
        let scope = match cur.first().and_then(|r| r.first().cloned().flatten()) {
            // Same scope as `list_schemas`: the session database, else the account.
            Some(db) => format!("in database {}", quote_ident(ConnectorKind::Snowflake, &db)),
            None => "in account".to_string(),
        };
        let sql = format!("show terse objects like {} {scope} limit {}", quote_literal(&format!("%{term}%")), (limit * 4).max(200));
        // SHOW TERSE OBJECTS: created_on, name, kind, database_name, schema_name
        let mut hits: Vec<DbObject> = self
            .0
            .run_small(&sql)
            .await?
            .into_iter()
            .filter_map(|r| {
                let g = |i: usize| r.get(i).cloned().flatten();
                let sch = g(4)?;
                if sch == "INFORMATION_SCHEMA" {
                    return None;
                }
                let k = g(2).unwrap_or_default();
                Some(DbObject {
                    schema: format!("{}.{sch}", g(3)?),
                    name: g(1)?,
                    kind: if k.contains("VIEW") { if k.contains("MATERIALIZED") { ObjectKind::MaterializedView } else { ObjectKind::View } } else if k.contains("EXTERNAL") { ObjectKind::ForeignTable } else { ObjectKind::Table },
                    comment: None,
                    row_estimate: None,
                })
            })
            .filter(|o| databrain_connector_core::object_matches(query, &o.schema, &o.name))
            .collect();
        databrain_connector_core::rank_matches(query, &mut hits, limit);
        Ok(hits)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let (db, sch) = schema.split_once('.').ok_or_else(|| ConnectorError::query("expected database.schema"))?;
        let sql = format!(
            "select table_name, table_type, comment, row_count from {}.information_schema.tables where table_schema = {} order by table_type, table_name",
            quote_ident(ConnectorKind::Snowflake, db),
            quote_literal(sch)
        );
        Ok(self
            .0
            .run_small(&sql)
            .await?
            .into_iter()
            .filter_map(|r| {
                let g = |i: usize| r.get(i).cloned().flatten();
                let t = g(1).unwrap_or_default();
                Some(DbObject {
                    schema: schema.to_string(),
                    name: g(0)?,
                    kind: if t.contains("VIEW") { if t.contains("MATERIALIZED") { ObjectKind::MaterializedView } else { ObjectKind::View } } else if t.contains("EXTERNAL") { ObjectKind::ForeignTable } else { ObjectKind::Table },
                    comment: g(2),
                    row_estimate: g(3).and_then(|n| n.parse().ok()),
                })
            })
            .collect())
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let object = self
            .list_objects(schema)
            .await?
            .into_iter()
            .find(|o| o.name == name)
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let columns = self.columns(schema, Some(name)).await?.into_iter().next().map(|t| t.columns).unwrap_or_default();
        let full = format!("{}.{}", databrain_connector_core::quote_path(ConnectorKind::Snowflake, schema), quote_ident(ConnectorKind::Snowflake, name));
        let kind = if matches!(object.kind, ObjectKind::View | ObjectKind::MaterializedView) { "view" } else { "table" };
        let ddl = self
            .0
            .run_small(&format!("select get_ddl('{kind}', {})", quote_literal(&full)))
            .await
            .ok()
            .and_then(|r| r.into_iter().next().and_then(|r| r.into_iter().next().flatten()));
        let foreign_keys = self.fks(&full).await.unwrap_or_default();
        Ok(ObjectDetail { object, columns, ddl, foreign_keys })
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        self.columns(schema, None).await
    }
}

impl SfSession {
    async fn columns(&self, schema: &str, table: Option<&str>) -> Result<Vec<TableColumns>> {
        let (db, sch) = schema.split_once('.').ok_or_else(|| ConnectorError::query("expected database.schema"))?;
        let mut sql = format!(
            "select table_name, column_name, data_type, character_maximum_length, numeric_precision, numeric_scale, is_nullable, column_default, comment \
             from {}.information_schema.columns where table_schema = {}",
            quote_ident(ConnectorKind::Snowflake, db),
            quote_literal(sch)
        );
        if let Some(t) = table {
            sql.push_str(&format!(" and table_name = {}", quote_literal(t)));
        }
        sql.push_str(" order by table_name, ordinal_position");
        let mut out: Vec<TableColumns> = Vec::new();
        for r in self.0.run_small(&sql).await? {
            let g = |i: usize| r.get(i).cloned().flatten();
            let base = g(2).unwrap_or_default();
            let data_type = match base.as_str() {
                "TEXT" => g(3).map(|l| format!("VARCHAR({l})")).unwrap_or("VARCHAR".into()),
                "NUMBER" => format!("NUMBER({},{})", g(4).unwrap_or("38".into()), g(5).unwrap_or("0".into())),
                _ => base,
            };
            let table = g(0).unwrap_or_default();
            let col = ColumnInfo { name: g(1).unwrap_or_default(), data_type, nullable: g(6).as_deref() != Some("NO"), is_primary_key: false, default: g(7), comment: g(8) };
            match out.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => out.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        // Primary keys (informational constraints) for single-table describe.
        if let Some(t) = table {
            let full = format!("{}.{}", databrain_connector_core::quote_path(ConnectorKind::Snowflake, schema), quote_ident(ConnectorKind::Snowflake, t));
            if let Ok(pk) = self.0.run_small(&format!("show primary keys in table {full}")).await {
                let pk_cols: Vec<String> = pk.into_iter().filter_map(|r| r.get(4).cloned().flatten()).collect();
                if let Some(tc) = out.first_mut() {
                    for c in &mut tc.columns {
                        c.is_primary_key = pk_cols.contains(&c.name);
                    }
                }
            }
        }
        Ok(out)
    }

    async fn fks(&self, full: &str) -> Result<Vec<ForeignKey>> {
        // SHOW IMPORTED KEYS columns: pk_database, pk_schema, pk_table, pk_column, fk_database, fk_schema, fk_table, fk_column, key_sequence, ..., fk_name
        let rows = self.0.run_small(&format!("show imported keys in table {full}")).await?;
        let mut out: Vec<(String, ForeignKey)> = Vec::new();
        for r in rows {
            let g = |i: usize| r.get(i + 1).cloned().flatten().unwrap_or_default();
            let fk_name = r.get(13).cloned().flatten().unwrap_or_default();
            match out.iter_mut().find(|(n, _)| *n == fk_name) {
                Some((_, fk)) => {
                    fk.columns.push(g(7));
                    fk.ref_columns.push(g(3));
                }
                None => out.push((fk_name, ForeignKey { columns: vec![g(7)], ref_schema: format!("{}.{}", g(0), g(1)), ref_table: g(2), ref_columns: vec![g(3)] })),
            }
        }
        Ok(out.into_iter().map(|(_, f)| f).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;

    #[test]
    fn account_and_host() {
        let mut c = ConnectionConfig::new(ConnectorKind::Snowflake, AuthMethod::Password { user: "u".into() });
        c.options.insert("account".into(), "xy12345.eu-west-1".into());
        let a = account(&c).unwrap();
        assert_eq!(host(&c, &a), "xy12345.eu-west-1.snowflakecomputing.com");
        assert_eq!(jwt_account(&a), "XY12345");
        c.options.insert("account".into(), "https://myorg-acct.snowflakecomputing.com/".into());
        assert_eq!(account(&c).unwrap(), "myorg-acct");
    }

    #[test]
    fn converts_cells() {
        let (t, db) = col_type(&json!({"type": "fixed", "precision": 10, "scale": 0}));
        assert_eq!((t, db.as_str()), (ColType::Int64, "number(10)"));
        assert_eq!(col_type(&json!({"type": "fixed", "precision": 10, "scale": 2})).0, ColType::Utf8);
        assert_eq!(sf_cell(ColType::Date, &json!("19723")), Value::Date(19723));
        assert_eq!(sf_cell(ColType::Timestamp, &json!("1700000000.123000000")), Value::Timestamp(1_700_000_000_123_000));
        assert_eq!(sf_cell(ColType::TimestampTz, &json!("1700000000.5 1860")), Value::TimestampTz(1_700_000_000_500_000));
        assert_eq!(sf_cell(ColType::Bool, &json!("1")), Value::Bool(true));
        assert_eq!(sf_cell(ColType::Utf8, &json!(null)), Value::Null);
    }

    #[test]
    fn oauth_defaults() {
        let mut c = ConnectionConfig::new(ConnectorKind::Snowflake, AuthMethod::OauthBrowser(Default::default()));
        c.options.insert("account".into(), "myorg-acct".into());
        c.options.insert("role".into(), "ANALYST".into());
        let o = SnowflakeConnector.auth_context(&c).oauth.unwrap();
        assert_eq!(o.authorize_url, "https://myorg-acct.snowflakecomputing.com/oauth/authorize");
        assert_eq!(o.scopes, vec!["session:role:ANALYST", "refresh_token"]);
        assert_eq!(o.client_auth, ClientAuthStyle::Basic);
    }

    /// Live: DATABRAIN_SF_ACCOUNT, _USER, _PASSWORD (optional _WAREHOUSE).
    #[tokio::test]
    async fn live_roundtrip() {
        let (Ok(acct), Ok(user), Ok(pw)) = (std::env::var("DATABRAIN_SF_ACCOUNT"), std::env::var("DATABRAIN_SF_USER"), std::env::var("DATABRAIN_SF_PASSWORD")) else {
            eprintln!("skipping: DATABRAIN_SF_* not set");
            return;
        };
        use databrain_auth::InlineCredentialSource;
        let mut c = ConnectionConfig::new(ConnectorKind::Snowflake, AuthMethod::Password { user: user.clone() });
        c.options.insert("account".into(), acct);
        if let Ok(w) = std::env::var("DATABRAIN_SF_WAREHOUSE") {
            c.options.insert("warehouse".into(), w);
        }
        let creds = Arc::new(InlineCredentialSource::new(AuthMethod::Password { user }, Some(pw.into())));
        let s = SnowflakeConnector.connect(&c, creds).await.unwrap();
        let r = s.execute("select 1 as a, 'x' as b, current_timestamp() as c, 1.5::number(5,2) as d", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), 1);
        let r = s.execute("select seq4() from table(generator(rowcount => 50000))", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), 50000);
    }
}
