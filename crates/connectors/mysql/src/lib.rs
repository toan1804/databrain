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
    /// changing it once the client says it can), the new one is set: MySQL's
    /// `ALTER USER CURRENT_USER() IDENTIFIED BY …` (with `REPLACE` the
    /// current one where the server takes it), else MariaDB's `SET PASSWORD
    /// = PASSWORD(…)` (its session refuses ALTER USER, 1820). Quotes are
    /// doubled, which reads the same whatever `sql_mode` says; a backslash
    /// is safe only once `sql_mode` is set to NO_BACKSLASH_ESCAPES, which
    /// MariaDB's session allows and MySQL's doesn't: there one in the new
    /// password is refused (in the current one: no `REPLACE`).
    async fn connect_changing_password(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>, new: &str) -> Result<()> {
        use mysql_async::prelude::Queryable;
        let (user, password) = credentials(&*creds).await?;
        let ssl = ssl_opts(cfg)?;
        let mut conn = match connect_raw(expired_opts(cfg, user.clone(), password.clone(), ssl)).await {
            Ok(c) => c,
            Err(e) if cfg.ssl_mode == SslMode::Prefer && e.server_has_no_tls() => connect_with_timeout(expired_opts(cfg, user, password.clone(), None)).await?,
            Err(e) => return Err(e.into_error()),
        };
        let q = databrain_connector_core::quote_literal;
        let code = |e: &databrain_connector_core::ConnectorError, n: &str| e.code.as_deref().is_some_and(|c| c.split_whitespace().next() == Some(n));
        let result = async {
            // MariaDB: allowed, and then a backslash is itself. MySQL: refused (1820).
            let plain = conn.query_drop("SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'").await.is_ok();
            if !plain && new.contains('\\') {
                return Err(databrain_connector_core::ConnectorError::config(
                    "A new password with a backslash (\\) can't be set while the old one has expired: set one without, then change it again",
                ));
            }
            let alter = format!("ALTER USER CURRENT_USER() IDENTIFIED BY {}", q(new));
            let first = if plain || !password.contains('\\') {
                conn.query_drop(format!("{alter} REPLACE {}", q(&password))).await.map_err(error::map_err)
            } else {
                conn.query_drop(alter.clone()).await.map_err(error::map_err)
            };
            let r = match first {
                Err(e) if code(&e, "1064") => conn.query_drop(alter).await.map_err(error::map_err),
                r => r,
            };
            match r {
                // MariaDB's session for an expired password takes only SET.
                Err(e) if code(&e, "1820") => conn.query_drop(format!("SET PASSWORD = PASSWORD({})", q(new))).await.map_err(error::map_err),
                r => r,
            }
        }
        .await;
        let _ = conn.disconnect().await;
        // A syntax error's message quotes the statement (the passwords): not shown.
        result.map_err(|e| {
            if e.code.as_deref().is_some_and(|c| c.starts_with("1064 ")) {
                databrain_connector_core::ConnectorError::query(
                    "This server doesn't take ALTER USER … IDENTIFIED BY or SET PASSWORD (MySQL 5.7.6 or MariaDB 10.4 and later do)",
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

    /// A server from the environment: `DATABRAIN_MYSQL_*` or `DATABRAIN_MARIADB_*`.
    struct Server(&'static str);

    impl Server {
        fn var(&self, k: &str) -> Option<String> {
            std::env::var(format!("DATABRAIN_{}_{k}", self.0)).ok()
        }
        fn cfg(&self, user: &str) -> ConnectionConfig {
            let mut c = ConnectionConfig::new(ConnectorKind::Mysql, AuthMethod::Password { user: user.into() });
            c.host = Some(self.var("HOST").unwrap_or_else(|| panic!("live test: DATABRAIN_{}_HOST not set (run with --ignored)", self.0)));
            c.port = self.var("PORT").and_then(|p| p.parse().ok());
            c
        }
    }

    fn creds(user: &str, password: &str) -> Arc<dyn CredentialSource> {
        Arc::new(InlineCredentialSource::new(AuthMethod::Password { user: user.into() }, Some(secrecy::SecretString::from(password.to_string()))))
    }

    /// The options for the expired sign-in run nothing the session would
    /// refuse: no database, no init statements, the settings the driver
    /// would otherwise SELECT given, and the capability sent.
    #[test]
    fn expired_sign_in_runs_nothing_else() {
        let mut c = ConnectionConfig::new(ConnectorKind::Mysql, AuthMethod::Password { user: "u".into() });
        c.database = Some("app".into());
        c.read_only = true;
        let o = expired_opts(&c, "u".into(), "p".into(), None);
        assert!(o.can_handle_expired_passwords());
        assert_eq!(o.db_name(), None);
        assert!(o.init().is_empty());
        assert!(o.max_allowed_packet().is_some() && o.wait_timeout().is_some());
        assert!(!o.prefer_socket());
        assert!(!build_opts(&c, "u".into(), "p".into(), None).can_handle_expired_passwords(), "only the expired sign-in sends it");
    }

    /// Live: an expired password is refused (or kept to the change) and
    /// told apart; signing in with it to change it works, the new password
    /// then signs in as usual and the old one doesn't; a quote is kept as
    /// typed; a backslash too where the session allows SET (MariaDB), else
    /// refused before anything is changed (MySQL); a wrong current password
    /// is refused.
    async fn expired_password_is_changed_at_sign_in(srv: Server, user: &str, backslash_allowed: bool) {
        let root = srv.var("USER").unwrap_or_else(|| "root".into());
        let admin = MysqlConnector.connect(&srv.cfg(&root), creds(&root, &srv.var("PASSWORD").unwrap_or_default())).await.unwrap();
        let run = |sql: String| {
            let s = &admin;
            async move { s.execute(databrain_connector_core::Statement::internal(&sql), ExecOptions::default()).await.unwrap().collect().await.unwrap() }
        };
        run(format!("DROP USER IF EXISTS '{user}'@'%'")).await;
        run(format!("CREATE USER '{user}'@'%' IDENTIFIED BY 'Old_pw_1'")).await;
        run(format!("ALTER USER '{user}'@'%' PASSWORD EXPIRE")).await;

        // MySQL refuses the sign-in (1862); MariaDB signs in to a session
        // that refuses every statement (1820): either is told apart.
        let e = match MysqlConnector.connect(&srv.cfg(user), creds(user, "Old_pw_1")).await {
            Err(e) => e,
            Ok(s) => s.server_version().await.expect_err("refused while expired"),
        };
        assert!(e.is_password_expired(), "{e:?}");
        MysqlConnector.connect_changing_password(&srv.cfg(user), creds(user, "Old_pw_1"), "New_pw_2").await.unwrap();
        let s = MysqlConnector.connect(&srv.cfg(user), creds(user, "New_pw_2")).await.unwrap();
        assert!(s.server_version().await.is_ok());
        assert!(MysqlConnector.connect(&srv.cfg(user), creds(user, "Old_pw_1")).await.is_err(), "the old one no longer signs in");

        run(format!("ALTER USER '{user}'@'%' PASSWORD EXPIRE")).await;
        let odd = "N'e\"w_pw_3";
        MysqlConnector.connect_changing_password(&srv.cfg(user), creds(user, "New_pw_2"), odd).await.unwrap();
        assert!(MysqlConnector.connect(&srv.cfg(user), creds(user, odd)).await.is_ok());

        run(format!("ALTER USER '{user}'@'%' PASSWORD EXPIRE")).await;
        let slash = r"N\e\w_pw_4";
        let r = MysqlConnector.connect_changing_password(&srv.cfg(user), creds(user, odd), slash).await;
        if backslash_allowed {
            r.unwrap();
            let s = MysqlConnector.connect(&srv.cfg(user), creds(user, slash)).await.expect("kept as typed");
            assert!(s.server_version().await.is_ok());
            run(format!("ALTER USER '{user}'@'%' PASSWORD EXPIRE")).await;
        } else {
            assert!(r.unwrap_err().message.contains("backslash"));
        }
        let current = if backslash_allowed { slash } else { odd };
        assert!(MysqlConnector.connect_changing_password(&srv.cfg(user), creds(user, "wrong"), "Other_pw_5").await.is_err());
        assert!(MysqlConnector.connect_changing_password(&srv.cfg(user), creds(user, current), "Other_pw_5").await.is_ok(), "still the current one");
        run(format!("DROP USER '{user}'@'%'")).await;
    }

    #[ignore = "live: needs DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_expired_password_is_changed_at_sign_in() {
        expired_password_is_changed_at_sign_in(Server("MYSQL"), "db_expired", false).await;
    }

    #[ignore = "live: needs DATABRAIN_MARIADB_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_mariadb_expired_password_is_changed_at_sign_in() {
        expired_password_is_changed_at_sign_in(Server("MARIADB"), "db_expired_m", true).await;
    }
}
