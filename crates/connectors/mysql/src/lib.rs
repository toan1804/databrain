//! MySQL / MariaDB connector built on `mysql_async` (text protocol).

mod config;
mod error;
mod exec;
mod schema;
mod session;
mod types;

use crate::config::Opener;
use crate::config::build_opts;
use crate::config::connect_raw;
use crate::config::connect_with_timeout;
use crate::config::credentials;
use crate::config::ssl_opts;
use async_trait::async_trait;
use databrain_auth::AuthMethodKind;
use databrain_auth::CredentialSource;
use databrain_connector_core::Capabilities;
use databrain_connector_core::ConnectionConfig;
use databrain_connector_core::Connector;
use databrain_connector_core::ConnectorInfo;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::FieldSpec;
use databrain_connector_core::Result;
use databrain_connector_core::Session;
use databrain_connector_core::SslMode;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use tokio::sync::Mutex;

pub use session::MysqlSession;

#[derive(Debug, Default)]
pub struct MysqlConnector;

impl MysqlConnector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Connector for MysqlConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Mysql,
            traits: ConnectorKind::Mysql.traits(),
            display_name: "MySQL / MariaDB",
            default_port: Some(3306),
            uses_file: false,
            auth_methods: vec![AuthMethodKind::Password],
            capabilities: Capabilities { ssh: true },
            fields: vec![
                FieldSpec::new("host", "Host").required().placeholder("localhost"),
                FieldSpec::new("port", "Port").placeholder("3306"),
                FieldSpec::new("database", "Database").placeholder("(optional)"),
                FieldSpec::new(config::SSL_ROOT_CERT, "CA certificate").placeholder("/path/to/ca.pem").help(
                    "PEM file of the CA that signed the server's certificate: with SSL Prefer or Require the certificate must chain to it, with Verify its host name must match too.",
                ),
            ],
            note: None,
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let (user, password) = credentials(&*creds).await?;
        let ssl = ssl_opts(cfg)?;
        let tls = ssl.is_some();
        let opts = build_opts(cfg, user.clone(), password.clone(), ssl);
        let (conn, tls) = match connect_raw(opts).await {
            Ok(c) => (c, tls),
            // "Prefer": without TLS only when the server has none. Not on
            // a refused password or a TLS error: that would send the
            // password again in plaintext where TLS was expected.
            Err(e) if cfg.ssl_mode == SslMode::Prefer && e.server_has_no_tls() => (connect_with_timeout(build_opts(cfg, user, password, None)).await?, false),
            Err(e) => return Err(e.into_error()),
        };
        Ok(Box::new(MysqlSession {
            conn_id: Arc::new(AtomicU32::new(conn.id())),
            conn: Arc::new(Mutex::new(conn)),
            read_only: cfg.read_only,
            opener: Arc::new(Opener { cfg: cfg.clone(), creds, tls }),
            state: Default::default(),
        }))
    }
}
