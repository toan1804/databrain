//! The Databricks connector: configuration, auth context and connecting.

use std::sync::Arc;

use async_trait::async_trait;
use databrain_auth::{AuthContext, AuthMethod, AuthMethodKind, CliKind, ClientAuthStyle, CredentialSource, OAuthConfig};
use databrain_connector_core::{Capabilities, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, FieldSpec, Result, Session};
use tokio::sync::Mutex;

use crate::common::{http, normalize_host};

use super::DEFAULT_CLIENT_ID;
use super::client::{DbxSession, Inner};
use super::stream::FETCH_PARALLEL;

const DEFAULT_REDIRECT_PORT: u16 = 8020;

#[derive(Debug, Default)]
pub struct DatabricksConnector;

impl DatabricksConnector {
    pub fn new() -> Self {
        Self
    }
}

pub(super) fn warehouse_id(cfg: &ConnectionConfig) -> Result<String> {
    if let Some(id) = cfg.opt("warehouse_id") {
        return Ok(id.to_string());
    }
    let path = cfg.opt("http_path").ok_or_else(|| ConnectorError::config("Databricks needs the SQL warehouse HTTP path (e.g. /sql/1.0/warehouses/abc123)"))?;
    path.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()).map(str::to_string).ok_or_else(|| ConnectorError::config("invalid HTTP path"))
}

#[async_trait]
impl Connector for DatabricksConnector {
    fn validate(&self, cfg: &ConnectionConfig) -> std::result::Result<(), String> {
        if cfg.opt("http_path").is_none() && cfg.opt("warehouse_id").is_none() {
            return Err("SQL warehouse HTTP path is required".into());
        }
        Ok(())
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Databricks,
            traits: ConnectorKind::Databricks.traits(),
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

#[cfg(test)]
mod tests {
    use databrain_connector_core::ExecOptions;

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

    /// Live: DATABRAIN_DBX_HOST, _HTTP_PATH, _TOKEN.
    #[ignore = "live: needs DATABRAIN_DBX_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_roundtrip() {
        let (Ok(host), Ok(path), Ok(token)) =
            (std::env::var("DATABRAIN_DBX_HOST"), std::env::var("DATABRAIN_DBX_HTTP_PATH"), std::env::var("DATABRAIN_DBX_TOKEN"))
        else {
            panic!("live test: DATABRAIN_DBX_* not set (run with --ignored)");
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
