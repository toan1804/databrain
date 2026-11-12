//! Microsoft SQL Server / Azure SQL connector built on `tiberius`.
//!
//! Auth: SQL login, Windows (NTLM, `DOMAIN\user`), and Microsoft Entra ID
//! tokens (browser, device code, service principal or `az login`).
//! Cancellation drops the connection (TDS attention is not supported by the
//! driver); the next statement reconnects transparently. At the row limit a
//! plain read stops on the server (`SET ROWCOUNT`); other statements stop
//! reading and replace the connection, unless it carries session state
//! (`databrain_connector_core::ConnState`), when the rest is read instead.

mod config;
mod error;
mod exec;
mod schema;
mod session;
mod types;

use crate::config::open;
use async_trait::async_trait;
use databrain_auth::AuthContext;
use databrain_auth::AuthMethodKind;
use databrain_auth::CliKind;
use databrain_auth::ClientAuthStyle;
use databrain_auth::Credential;
use databrain_auth::CredentialSource;
use databrain_auth::OAuthConfig;
use databrain_connector_core::Capabilities;
use databrain_connector_core::ConnectionConfig;
use databrain_connector_core::Connector;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ConnectorInfo;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::FieldSpec;
use databrain_connector_core::Result;
use databrain_connector_core::Session;
use databrain_connector_core::SslMode;
use secrecy::ExposeSecret;
use std::sync::Arc;
use tiberius::AuthMethod as TdsAuth;
use tiberius::Config;
use tiberius::EncryptionLevel;
use tokio::sync::Mutex;

pub use session::MssqlSession;

const ENTRA_SCOPE: &str = "https://database.windows.net//.default";

#[derive(Debug, Default)]
pub struct MssqlConnector;

impl MssqlConnector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Connector for MssqlConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Mssql,
            traits: ConnectorKind::Mssql.traits(),
            display_name: "SQL Server",
            default_port: Some(1433),
            uses_file: false,
            auth_methods: vec![
                AuthMethodKind::Password,
                AuthMethodKind::OauthBrowser,
                AuthMethodKind::DeviceCode,
                AuthMethodKind::ClientCredentials,
                AuthMethodKind::CloudCli,
            ],
            capabilities: Capabilities { ssh: true },
            fields: vec![
                FieldSpec::new("host", "Host").required().placeholder("localhost or myserver.database.windows.net"),
                FieldSpec::new("port", "Port").placeholder("1433"),
                FieldSpec::new("database", "Database")
                    .placeholder("(all databases)")
                    .help("Empty: the explorer lists every database you can access, like catalogs; name tables as database.schema.table"),
                FieldSpec::new("instance", "Instance name").placeholder("(optional, e.g. SQLEXPRESS)"),
                FieldSpec::new("tenant", "Entra tenant").placeholder("organizations").help("For Microsoft Entra ID sign-in"),
                FieldSpec::new("trust_cert", "Trust server certificate").placeholder("false"),
            ],
            note: Some("Use DOMAIN\\user for Windows (NTLM) logins. Entra ID sign-in needs an app registration client ID."),
        }
    }

    fn auth_context(&self, cfg: &ConnectionConfig) -> AuthContext {
        let p = cfg.auth.oauth_params().cloned().unwrap_or_default();
        let tenant = p.tenant.clone().or(cfg.opt("tenant").map(str::to_string)).unwrap_or_else(|| "organizations".into());
        let base = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0");
        let mut scopes: Vec<String> =
            p.scopes.as_deref().map(|s| s.split_whitespace().map(str::to_string).collect()).unwrap_or_else(|| vec![ENTRA_SCOPE.into()]);
        if !matches!(cfg.auth, databrain_auth::AuthMethod::ClientCredentials(_)) && !scopes.iter().any(|s| s == "offline_access") {
            scopes.push("offline_access".into());
        }
        AuthContext {
            oauth: Some(OAuthConfig {
                provider: "Microsoft Entra ID".into(),
                authorize_url: format!("{base}/authorize"),
                token_url: format!("{base}/token"),
                device_url: Some(format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode")),
                revoke_url: None,
                client_id: p.client_id.unwrap_or_default(),
                client_secret: None,
                scopes,
                redirect_host: "localhost".into(),
                redirect_port: p.redirect_port,
                redirect_path: "/".into(),
                extra_authorize_params: p.user.map(|u| vec![("login_hint".to_string(), u)]).unwrap_or_default(),
                client_auth: ClientAuthStyle::Body,
            }),
            cli: Some(CliKind::Azure { resource: "https://database.windows.net/".into() }),
            google_scopes: vec![],
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let mut c = Config::new();
        c.host(cfg.host_or_default());
        if let Some(p) = cfg.port {
            c.port(p);
        }
        if let Some(db) = cfg.database.as_deref().filter(|d| !d.is_empty()) {
            c.database(db);
        }
        if let Some(i) = cfg.opt("instance") {
            c.instance_name(i);
        }
        c.application_name("DataBrain");
        let trust = cfg.opt("trust_cert").is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1");
        match cfg.ssl_mode {
            SslMode::Disable => c.encryption(EncryptionLevel::NotSupported),
            SslMode::Prefer => {
                c.encryption(EncryptionLevel::On);
                c.trust_cert();
            }
            SslMode::Require => {
                c.encryption(EncryptionLevel::Required);
                c.trust_cert();
            }
            SslMode::VerifyFull => {
                c.encryption(EncryptionLevel::Required);
                if trust {
                    c.trust_cert();
                }
            }
        }
        if trust {
            c.trust_cert();
        }
        c.authentication(match creds.get().await? {
            #[cfg(windows)]
            Credential::Password { user, password } if user.contains('\\') => TdsAuth::windows(&user, password.expose_secret()),
            #[cfg(not(windows))]
            Credential::Password { user, .. } if user.contains('\\') => {
                return Err(ConnectorError::config("Windows (DOMAIN\\user) logins are only supported on Windows; use a SQL login or Entra ID"));
            }
            Credential::Password { user, password } => TdsAuth::sql_server(user, password.expose_secret()),
            Credential::Bearer { token, .. } => TdsAuth::aad_token(token.expose_secret()),
            _ => return Err(ConnectorError::config("unsupported authentication method for SQL Server")),
        });
        let use_browser = cfg.opt("instance").is_some() && cfg.port.is_none();
        let all_dbs = cfg.database.as_deref().is_none_or(|d| d.trim().is_empty());
        let conn = if cfg.ssl_mode == SslMode::Prefer {
            // tiberius panics when the server can't encrypt; Prefer then
            // means a plain connection (as encrypt=false in other drivers).
            match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(open(&c, use_browser))).await {
                Ok(r) => r?,
                Err(_) => {
                    c.encryption(EncryptionLevel::NotSupported);
                    open(&c, use_browser).await?
                }
            }
        } else {
            open(&c, use_browser).await?
        };
        Ok(Box::new(MssqlSession {
            config: c,
            use_browser,
            conn: Arc::new(Mutex::new(Some(conn))),
            state: Default::default(),
            user_rowcount: Default::default(),
            all_dbs,
        }))
    }
}
