//! Oracle connector with two drivers:
//!
//! - **Thin** (default): a helper process using go-ora, downloaded on first
//!   use (see [`agent`]). No Oracle software needed.
//! - **Instant Client** (`driver = instant_client`): the `oracle` crate over
//!   ODPI-C, for features go-ora lacks (e.g. OS authentication). Needs
//!   Oracle Instant Client; its API is synchronous, so calls run on
//!   blocking threads.
//!
//! Catalog queries are the same for both drivers.

pub mod agent;
pub mod client;
mod config;
mod ddl;
mod error;
mod exec;
mod schema;
mod session;
mod types;

use crate::config::open;
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
use std::sync::Arc;

pub use config::uses_instant_client;
pub use session::OracleSession;

#[derive(Debug, Default)]
pub struct OracleConnector;

impl OracleConnector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Connector for OracleConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Oracle,
            traits: ConnectorKind::Oracle.traits(),
            display_name: "Oracle",
            default_port: Some(1521),
            uses_file: false,
            auth_methods: vec![AuthMethodKind::Password],
            capabilities: Capabilities { transactions: true, cancel: true, schemas: true, read_only_sessions: true, ssh: true },
            fields: vec![
                FieldSpec::new("host", "Host").required().placeholder("localhost"),
                FieldSpec::new("port", "Port").placeholder("1521"),
                FieldSpec::new("service_name", "Service name").required().placeholder("FREEPDB1 / ORCLPDB1"),
                FieldSpec::new("connect_string", "Connect string").placeholder("(optional) TNS alias or full descriptor").help("Overrides host/port/service"),
                FieldSpec::new("driver", "Driver").help("Thin needs no Oracle software; Instant Client is for OS authentication and other thick-only features"),
                FieldSpec::new("driver_options", "Driver options")
                    .placeholder("SSL=true; WALLET=/path/to/wallet; ENCRYPTION=REQUIRED")
                    .help("Thin driver: go-ora options separated by ';'"),
                FieldSpec::new("client_lib_dir", "Instant Client directory").placeholder("/opt/oracle/instantclient_23_3"),
            ],
            note: Some("The thin driver (default) needs no Oracle software; it is downloaded on first use (5 MB)."),
        }
    }

    /// The thin driver's helper is downloaded on first use (5 MB): not part
    /// of the connect time limit.
    async fn prepare(&self, cfg: &ConnectionConfig) -> Result<()> {
        if !uses_instant_client(cfg) {
            agent::ensure().await?;
        }
        Ok(())
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        Ok(Box::new(open(cfg, creds).await?))
    }
}
