//! MySQL / MariaDB connector built on `mysql_async` (text protocol).

mod config;
mod error;
mod exec;
mod schema;
mod session;
mod types;

use crate::config::build_opts;
use crate::config::connect_with_timeout;
use crate::config::ssl_opts;
use async_trait::async_trait;
use databrain_auth::AuthMethodKind;
use databrain_auth::Credential;
use databrain_auth::CredentialSource;
use databrain_connector_core::Capabilities;
use databrain_connector_core::ConnectionConfig;
use databrain_connector_core::Connector;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ConnectorInfo;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::ErrorKind;
use databrain_connector_core::FieldSpec;
use databrain_connector_core::Result;
use databrain_connector_core::Session;
use databrain_connector_core::SslMode;
use secrecy::ExposeSecret;
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
            capabilities: Capabilities { transactions: true, cancel: true, schemas: true, read_only_sessions: true, ssh: true },
            fields: vec![
                FieldSpec::new("host", "Host").required().placeholder("localhost"),
                FieldSpec::new("port", "Port").placeholder("3306"),
                FieldSpec::new("database", "Database").placeholder("(optional)"),
            ],
            note: None,
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let (user, password) = match creds.get().await? {
            Credential::Password { user, password } => (user, password.expose_secret().to_string()),
            _ => return Err(ConnectorError::config("MySQL requires user/password authentication")),
        };
        let opts = build_opts(cfg, user.clone(), password.clone(), ssl_opts(cfg.ssl_mode));
        let conn = match connect_with_timeout(opts.clone()).await {
            Ok(c) => (c, opts),
            // "Prefer": fall back to plaintext if the server has no TLS.
            Err(e) if cfg.ssl_mode == SslMode::Prefer && e.kind == ErrorKind::Connection => {
                let plain = build_opts(cfg, user, password, None);
                (connect_with_timeout(plain.clone()).await?, plain)
            }
            Err(e) => return Err(e),
        };
        let (conn, opts) = conn;
        Ok(Box::new(MysqlSession {
            conn_id: Arc::new(AtomicU32::new(conn.id())),
            conn: Arc::new(Mutex::new(conn)),
            read_only: cfg.read_only,
            opts,
            state: Default::default(),
        }))
    }
}
