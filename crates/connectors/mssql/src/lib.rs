//! Microsoft SQL Server / Azure SQL connector built on `tiberius`.
//!
//! Auth: SQL login, Windows (NTLM, `DOMAIN\user`), and Microsoft Entra ID
//! tokens (browser, device code, service principal or `az login`).
//! Cancellation drops the connection (TDS attention is not supported by the
//! driver); the next statement reconnects transparently. At the row limit a
//! plain read stops on the server (`SET ROWCOUNT`); other statements stop
//! reading and replace the connection, unless it carries session state
//! (`databrain_connector_core::ConnState`), when the rest is read instead.

mod bulk;
mod config;
mod error;
mod exec;
mod schema;
mod session;
mod types;

use crate::config::Opener;
use async_trait::async_trait;
use databrain_auth::AuthContext;
use databrain_auth::AuthMethodKind;
use databrain_auth::CliKind;
use databrain_auth::ClientAuthStyle;
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
use std::sync::Arc;
use tiberius::Config;
use tiberius::EncryptionLevel;
use tokio::sync::Mutex;

pub use bulk::bench_rows;
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
        let (c, use_browser) = tds_config(cfg);
        let all_dbs = cfg.database.as_deref().is_none_or(|d| d.trim().is_empty());
        // The login is asked for at each connection (reconnects too): a kept
        // Entra token would expire after about an hour.
        let mut opener = Opener { config: c, use_browser, creds };
        let conn = open_or_plain(&mut opener, cfg.ssl_mode).await?;
        Ok(Box::new(MssqlSession {
            opener: Arc::new(opener),
            conn: Arc::new(Mutex::new(Some(conn))),
            state: Default::default(),
            user_rowcount: Default::default(),
            all_dbs,
            db_cache: Default::default(),
        }))
    }

    /// SQL Server takes a new password for an expired one (18487) or one
    /// that must be changed (18488) only as part of the sign-in: LOGIN7's
    /// ChangePassword (vendor/tiberius/DATABRAIN.md). SQL Server logins only:
    /// a Windows or Entra ID password is changed with their own tools. It
    /// signs in to the login's default database: a connection's database
    /// it can't open would fail the sign-in after the server had changed
    /// the password, which then wouldn't be saved. It is never sent without
    /// encryption unless the connection's SSL mode is Disable: under Prefer
    /// a server that can't encrypt is refused (the new password is the one
    /// kept, and the login's obfuscation is trivially reversed).
    async fn connect_changing_password(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>, new: &str) -> Result<()> {
        match creds.get().await? {
            databrain_auth::Credential::Password { user, .. } if !user.contains('\\') => {}
            databrain_auth::Credential::Password { .. } => {
                return Err(ConnectorError::unsupported("A Windows login's password is changed in Windows (or by an administrator), not from DataBrain"));
            }
            _ => return Err(ConnectorError::unsupported("Only a SQL Server login's password can be changed from DataBrain")),
        }
        let mut cfg = cfg.clone();
        cfg.database = None;
        let (mut c, use_browser) = tds_config(&cfg);
        c.new_password(new);
        let opener = Opener { config: c, use_browser, creds };
        match opener.open().await {
            Ok(conn) => {
                drop(conn);
                Ok(())
            }
            Err(e) if error::prefer_falls_back(cfg.ssl_mode, &e) => Err(ConnectorError::connection(
                "The server can't encrypt the connection, so the new password isn't sent: set the connection's SSL mode to Disable to send it anyway",
            )),
            Err(e) => Err(e),
        }
    }
}

/// The server settings for a connection (everything but the login), and
/// whether SQL Browser finds the port (a named instance without one).
fn tds_config(cfg: &ConnectionConfig) -> (Config, bool) {
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
    let use_browser = cfg.opt("instance").is_some() && cfg.port.is_none();
    (c, use_browser)
}

/// A connection; a server that can't encrypt gets a plain one under
/// Prefer (as encrypt=false in other drivers), which reconnects keep.
async fn open_or_plain(opener: &mut Opener, ssl: SslMode) -> Result<config::Conn> {
    match opener.open().await {
        Ok(conn) => Ok(conn),
        Err(e) if error::prefer_falls_back(ssl, &e) => {
            opener.config.encryption(EncryptionLevel::NotSupported);
            opener.open().await
        }
        Err(e) => Err(e),
    }
}
