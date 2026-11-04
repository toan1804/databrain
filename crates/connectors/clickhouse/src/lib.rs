//! ClickHouse connector over the HTTP interface (port 8123; 8443 with TLS).
//!
//! Each DataBrain session is a ClickHouse HTTP session (`session_id`), so
//! `SET`, temporary tables and `USE` carry over between statements. Results
//! are read as `TabSeparatedWithNamesAndTypes` and streamed line by line.
//! Metadata comes from the `system` tables, outside the session, so the
//! explorer never waits for a running query.

mod config;
mod error;
mod exec;
mod schema;
mod session;
mod types;

use crate::config::endpoint;
use crate::exec::Inner;
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
use secrecy::ExposeSecret;
use std::sync::Arc;
use std::time::Duration;

pub use session::ChSession;

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
