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
use crate::config::expired_opts;
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

    /// Signed in with the expired password (the server allows a session for
    /// changing it once the client says it can), `ALTER USER CURRENT_USER()
    /// IDENTIFIED BY …` sets the new one, with `REPLACE` the current one
    /// where the server takes it. That session takes no SET or SELECT, so
    /// `sql_mode` (whether a backslash escapes) can't be read or set: a
    /// quote is doubled, which reads the same either way, and a backslash
    /// in the new password is refused (the current one: no `REPLACE`).
    async fn connect_changing_password(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>, new: &str) -> Result<()> {
        use mysql_async::prelude::Queryable;
        if new.contains('\\') {
            return Err(databrain_connector_core::ConnectorError::config(
                "A new password with a backslash (\\) can't be set while the old one has expired: set one without, then change it again",
            ));
        }
        let (user, password) = credentials(&*creds).await?;
        let ssl = ssl_opts(cfg)?;
        let mut conn = match connect_raw(expired_opts(cfg, user.clone(), password.clone(), ssl)).await {
            Ok(c) => c,
            Err(e) if cfg.ssl_mode == SslMode::Prefer && e.server_has_no_tls() => connect_with_timeout(expired_opts(cfg, user, password.clone(), None)).await?,
            Err(e) => return Err(e.into_error()),
        };
        let q = databrain_connector_core::quote_literal;
        let result = async {
            let syntax = |e: &databrain_connector_core::ConnectorError| e.code.as_deref().is_some_and(|c| c.starts_with("1064 "));
            let plain = format!("ALTER USER CURRENT_USER() IDENTIFIED BY {}", q(new));
            if password.contains('\\') {
                return conn.query_drop(plain).await.map_err(error::map_err);
            }
            match conn.query_drop(format!("{plain} REPLACE {}", q(&password))).await.map_err(error::map_err) {
                Err(e) if syntax(&e) => conn.query_drop(plain).await.map_err(error::map_err),
                r => r,
            }
        }
        .await;
        let _ = conn.disconnect().await;
        // A syntax error's message quotes the statement (the passwords): not shown.
        result.map_err(|e| {
            if e.code.as_deref().is_some_and(|c| c.starts_with("1064 ")) {
                databrain_connector_core::ConnectorError::query(
                    "This server doesn't take ALTER USER … IDENTIFIED BY (MySQL 5.7.6 or MariaDB 10.4 and later do)",
                )
            } else {
                e
            }
        })
    }
}

#[cfg(test)]
mod expired_password_tests {
    use super::*;
    use databrain_auth::{AuthMethod, InlineCredentialSource};
    use databrain_connector_core::ExecOptions;

    fn cfg(user: &str) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(ConnectorKind::Mysql, AuthMethod::Password { user: user.into() });
        c.host = Some(std::env::var("DATABRAIN_MYSQL_HOST").expect("live test: DATABRAIN_MYSQL_HOST not set (run with --ignored)"));
        c.port = std::env::var("DATABRAIN_MYSQL_PORT").ok().and_then(|p| p.parse().ok());
        c
    }

    fn creds(user: &str, password: &str) -> Arc<dyn CredentialSource> {
        Arc::new(InlineCredentialSource::new(AuthMethod::Password { user: user.into() }, Some(secrecy::SecretString::from(password.to_string()))))
    }

    /// Live: an expired password is refused at sign-in and told apart
    /// (1862); signing in with it to change it works, the new password then
    /// signs in as usual, and one with a backslash and a quote is kept as
    /// typed.
    #[ignore = "live: needs DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_expired_password_is_changed_at_sign_in() {
        let root = std::env::var("DATABRAIN_MYSQL_USER").unwrap_or_else(|_| "root".into());
        let admin = MysqlConnector.connect(&cfg(&root), creds(&root, &std::env::var("DATABRAIN_MYSQL_PASSWORD").unwrap_or_default())).await.unwrap();
        let run = |sql: String| {
            let s = &admin;
            async move { s.execute(databrain_connector_core::Statement::internal(&sql), ExecOptions::default()).await.unwrap().collect().await.unwrap() }
        };
        run("DROP USER IF EXISTS 'db_expired'@'%'".into()).await;
        run("CREATE USER 'db_expired'@'%' IDENTIFIED BY 'Old_pw_1'".into()).await;
        run("ALTER USER 'db_expired'@'%' PASSWORD EXPIRE".into()).await;

        let e = MysqlConnector.connect(&cfg("db_expired"), creds("db_expired", "Old_pw_1")).await.err().expect("refused while expired");
        assert!(e.is_password_expired(), "{e:?}");
        MysqlConnector.connect_changing_password(&cfg("db_expired"), creds("db_expired", "Old_pw_1"), "New_pw_2").await.unwrap();
        let s = MysqlConnector.connect(&cfg("db_expired"), creds("db_expired", "New_pw_2")).await.unwrap();
        assert!(s.server_version().await.is_ok());
        assert!(MysqlConnector.connect(&cfg("db_expired"), creds("db_expired", "Old_pw_1")).await.is_err(), "the old one no longer signs in");

        // A quote is kept as typed; a backslash is refused before anything
        // signs in (whether it escapes depends on a sql_mode that session
        // can't read), the password unchanged.
        run("ALTER USER 'db_expired'@'%' PASSWORD EXPIRE".into()).await;
        let e = MysqlConnector.connect_changing_password(&cfg("db_expired"), creds("db_expired", "New_pw_2"), r"N\ew_pw").await.unwrap_err();
        assert!(e.message.contains("backslash"), "{e:?}");
        let odd = "N'e\"w_pw_3";
        MysqlConnector.connect_changing_password(&cfg("db_expired"), creds("db_expired", "New_pw_2"), odd).await.unwrap();
        assert!(MysqlConnector.connect(&cfg("db_expired"), creds("db_expired", odd)).await.is_ok());

        // A wrong current password is refused, nothing changed.
        run("ALTER USER 'db_expired'@'%' PASSWORD EXPIRE".into()).await;
        assert!(MysqlConnector.connect_changing_password(&cfg("db_expired"), creds("db_expired", "wrong"), "Other_pw_4").await.is_err());
        run("DROP USER 'db_expired'@'%'".into()).await;
    }
}
