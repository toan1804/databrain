//! PostgreSQL connector built on `tokio-postgres`.
//!
//! Execution strategy: the statement is first prepared to learn the result
//! column types, then executed with the simple-query (text) protocol. Text
//! results make every Postgres type displayable exactly as `psql` shows it
//! (numeric, arrays, json, ranges, ...), while the prepared column types let
//! common types become typed Arrow columns for sorting and filtering.

mod connect;
mod ddl;
mod error;
mod exec;
mod metadata;
mod redshift;
mod session;
mod types;
mod wkb;

pub use secrecy::ExposeSecret;

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, Credential, CredentialSource};
use databrain_connector_core::{
    Capabilities, ConnState, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, FieldSpec, Result, Session, SslMode,
};
use std::sync::Arc;
use std::time::Duration;

use connect::{Link, REDSHIFT_INIT, open_link, redshift_flavor, tls_connector};
use session::PgSession;

#[derive(Debug, Default)]
pub struct PostgresConnector;

impl PostgresConnector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Connector for PostgresConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Postgres,
            traits: ConnectorKind::Postgres.traits(),
            display_name: "PostgreSQL / Redshift",
            default_port: Some(5432),
            uses_file: false,
            auth_methods: vec![AuthMethodKind::Password, AuthMethodKind::CloudCli],
            capabilities: Capabilities { transactions: true, cancel: true, schemas: true, read_only_sessions: true, ssh: true },
            fields: vec![
                FieldSpec::new("flavor", "Database type"),
                FieldSpec::new("host", "Host").required().placeholder("localhost"),
                FieldSpec::new("port", "Port").placeholder("5432"),
                FieldSpec::new("database", "Database").placeholder("postgres"),
                FieldSpec::new("cluster_id", "Cluster ID").placeholder("from the host").help("IAM sign-in, provisioned clusters"),
                FieldSpec::new("workgroup", "Workgroup").placeholder("from the host").help("IAM sign-in, Redshift Serverless"),
                FieldSpec::new("aws_region", "AWS region").placeholder("from the host or profile"),
                FieldSpec::new("db_user", "Database user")
                    .placeholder("the IAM identity's own")
                    .help("IAM sign-in to a cluster as this user (redshift:GetClusterCredentials)"),
            ],
            note: None,
        }
    }

    fn auth_context(&self, cfg: &ConnectionConfig) -> databrain_auth::AuthContext {
        if !matches!(cfg.auth, databrain_auth::AuthMethod::CloudCli { .. }) {
            return Default::default();
        }
        let mut t = databrain_auth::cloud::RedshiftTarget::from_host(cfg.host_or_default());
        if let Some(c) = cfg.opt("cluster_id") {
            t.cluster = Some(c.to_string());
            t.workgroup = None;
        }
        if let Some(w) = cfg.opt("workgroup") {
            t.workgroup = Some(w.to_string());
            t.cluster = None;
        }
        if let Some(r) = cfg.opt("aws_region") {
            t.region = Some(r.to_string());
        }
        t.db_user = cfg.opt("db_user").map(str::to_string);
        t.database = cfg.database.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| "dev".into());
        databrain_auth::AuthContext { cli: Some(databrain_auth::CliKind::Redshift(t)), ..Default::default() }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let redshift = redshift_flavor(cfg);
        let mut pg = tokio_postgres::Config::new();
        pg.host(cfg.host_or_default())
            .port(cfg.port.unwrap_or(if redshift { 5439 } else { 5432 }))
            .dbname(cfg.database.as_deref().filter(|d| !d.is_empty()).unwrap_or(if redshift { "dev" } else { "postgres" }))
            .application_name("DataBrain")
            .connect_timeout(Duration::from_secs(15))
            .keepalives(true);

        // Temporary credentials (IAM): a refused sign-in is reported as
        // such, so the engine gets new ones and tries once more.
        let mut temporary = false;
        match creds.get().await? {
            Credential::Password { user, password } => {
                pg.user(&user);
                if !password.expose_secret().is_empty() {
                    pg.password(password.expose_secret());
                }
            }
            Credential::None => {
                return Err(ConnectorError::config("PostgreSQL requires a user name"));
            }
            Credential::Bearer { token, user } => {
                // e.g. cloud IAM tokens used as the password.
                pg.user(user.as_deref().unwrap_or_default());
                pg.password(token.expose_secret());
                temporary = true;
            }
            _ => {
                return Err(ConnectorError::config("unsupported authentication method for PostgreSQL"));
            }
        }

        // ISO dates make text results parseable regardless of server config.
        // Redshift refuses startup options: the same is set with SQL.
        let mut plain = pg.clone();
        if !redshift {
            let mut options = String::from("-c DateStyle=ISO,MDY");
            if cfg.read_only {
                options.push_str(" -c default_transaction_read_only=on");
            }
            pg.options(&options);
        }

        let mode = match cfg.ssl_mode {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
        };
        pg.ssl_mode(mode);
        plain.ssl_mode(mode);
        let tls = tls_connector(cfg.ssl_mode)?;
        let mut init = redshift.then(|| REDSHIFT_INIT.to_string());
        let (client, task) = match open_link(&pg, tls.clone(), init.as_deref(), temporary).await {
            // A Redshift endpoint the host didn't reveal (SSH tunnel, DNS alias).
            Err(e) if !redshift && e.message.contains("startup parameter") && e.message.contains("options") => {
                pg = plain;
                init = Some(REDSHIFT_INIT.to_string());
                open_link(&pg, tls.clone(), init.as_deref(), temporary).await?
            }
            r => r?,
        };
        let version: String = client.query_one("select version()", &[]).await.map(|r| r.get(0)).unwrap_or_default();
        Ok(Box::new(PgSession {
            link: Arc::new(Link { pg, tls: tls.clone(), init, temporary, current: std::sync::RwLock::new((Arc::new(client), task)), state: ConnState::new() }),
            tls,
            read_only: cfg.read_only,
            redshift: redshift || version.contains("Redshift"),
        }))
    }
}
