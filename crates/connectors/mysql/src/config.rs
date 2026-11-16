//! Connection options: TLS, init commands and connecting with a timeout.

use crate::error::map_err;
use databrain_auth::Credential;
use databrain_auth::CredentialSource;
use databrain_connector_core::ConnectionConfig;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ErrorKind;
use databrain_connector_core::Result;
use databrain_connector_core::SslMode;
use mysql_async::Conn;
use mysql_async::Opts;
use mysql_async::OptsBuilder;
use mysql_async::SslOpts;
use secrecy::ExposeSecret;
use std::sync::Arc;
use std::time::Duration;

/// The connection's option holding a CA certificate (PEM file) to verify
/// the server against: with it, Prefer and Require check the chain (not
/// the host name), as libpq's `sslrootcert` does; Verify checks both.
pub const SSL_ROOT_CERT: &str = "ssl_root_cert";

pub(crate) fn build_opts(cfg: &ConnectionConfig, user: String, password: String, ssl: Option<SslOpts>) -> Opts {
    let mut init = vec!["SET NAMES utf8mb4".to_string()];
    if cfg.read_only {
        init.push("SET SESSION TRANSACTION READ ONLY".to_string());
    }
    OptsBuilder::default()
        .ip_or_hostname(cfg.host_or_default())
        .tcp_port(cfg.port.unwrap_or(3306))
        .user(Some(user))
        .pass(Some(password).filter(|p| !p.is_empty()))
        .db_name(cfg.database.clone().filter(|d| !d.is_empty()))
        .prefer_socket(false)
        .init(init)
        .ssl_opts(ssl)
        .into()
}

/// TLS for the connection's SSL mode. With a CA file (`ssl_root_cert`)
/// the server's certificate must chain to it (only to it); Verify also
/// checks the host name. Without one, Prefer and Require encrypt without
/// checking the certificate (libpq's meaning), Verify uses the system's CAs.
pub(crate) fn ssl_opts(cfg: &ConnectionConfig) -> Result<Option<SslOpts>> {
    let root = cfg.opt(SSL_ROOT_CERT).map(str::trim).filter(|p| !p.is_empty());
    if let Some(p) = root
        && cfg.ssl_mode != SslMode::Disable
        && !std::path::Path::new(p).is_file()
    {
        return Err(ConnectorError::config(format!("CA certificate file not found: {p}")));
    }
    Ok(match (cfg.ssl_mode, root) {
        (SslMode::Disable, _) => None,
        (mode, Some(p)) => Some(
            SslOpts::default()
                .with_root_certs(vec![std::path::PathBuf::from(p).into()])
                .with_disable_built_in_roots(true)
                .with_danger_skip_domain_validation(mode != SslMode::VerifyFull),
        ),
        (SslMode::Prefer | SslMode::Require, None) => Some(SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)),
        (SslMode::VerifyFull, None) => Some(SslOpts::default()),
    })
}

/// User and password from the connection's credential source.
pub(crate) async fn credentials(creds: &dyn CredentialSource) -> Result<(String, String)> {
    match creds.get().await? {
        Credential::Password { user, password } => Ok((user, password.expose_secret().to_string())),
        _ => Err(ConnectorError::config("MySQL requires user/password authentication")),
    }
}

/// How the session opens another connection (to stop a statement, or in
/// place of one still receiving a result): the credential is read from its
/// source each time, not kept, so a changed or expiring one is fetched
/// again. `tls`: whether the session's own connection used TLS (Prefer
/// against a server without it doesn't).
pub(crate) struct Opener {
    pub(crate) cfg: ConnectionConfig,
    pub(crate) creds: Arc<dyn CredentialSource>,
    pub(crate) tls: bool,
}

impl Opener {
    pub(crate) async fn opts(&self) -> Result<Opts> {
        let (user, password) = credentials(&*self.creds).await?;
        let ssl = if self.tls { ssl_opts(&self.cfg)? } else { None };
        Ok(build_opts(&self.cfg, user, password, ssl))
    }

    /// A new connection, within `limit`.
    pub(crate) async fn open(&self, limit: Duration) -> Option<Conn> {
        // Reading the credential is within the limit too: a keychain prompt
        // that nobody answers must not hold up Stop.
        tokio::time::timeout(limit, async { Conn::new(self.opts().await.ok()?).await.ok() }).await.ok()?
    }
}

/// Why connecting failed: the server answered (with an error), or not in time.
pub(crate) enum ConnectFailure {
    Server(mysql_async::Error),
    TimedOut,
}

impl ConnectFailure {
    /// The server can't do TLS at all: the only case Prefer goes on
    /// without it. Not a refused sign-in, a certificate or handshake error,
    /// or a lost connection: those would send the credential in plaintext
    /// where TLS was expected to work.
    pub(crate) fn server_has_no_tls(&self) -> bool {
        matches!(self, ConnectFailure::Server(mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer)))
    }

    pub(crate) fn into_error(self) -> ConnectorError {
        match self {
            ConnectFailure::Server(e) => {
                let mut ce = map_err(e);
                if ce.kind == ErrorKind::Query {
                    ce.kind = ErrorKind::Connection;
                }
                ce
            }
            ConnectFailure::TimedOut => ConnectorError::connection("connection timed out"),
        }
    }
}

pub(crate) async fn connect_raw(opts: Opts) -> std::result::Result<Conn, ConnectFailure> {
    match tokio::time::timeout(Duration::from_secs(15), Conn::new(opts)).await {
        Ok(r) => r.map_err(ConnectFailure::Server),
        Err(_) => Err(ConnectFailure::TimedOut),
    }
}

pub(crate) async fn connect_with_timeout(opts: Opts) -> Result<Conn> {
    connect_raw(opts).await.map_err(ConnectFailure::into_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectorKind;

    fn cfg(mode: SslMode, root: Option<&str>) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(ConnectorKind::Mysql, AuthMethod::Password { user: "u".into() });
        c.ssl_mode = mode;
        if let Some(r) = root {
            c.options.insert(SSL_ROOT_CERT.into(), r.into());
        }
        c
    }

    /// Prefer goes on without TLS only when the server has none: a refused
    /// password (it would be sent again in plaintext), a timeout or another
    /// error doesn't.
    #[test]
    fn prefer_falls_back_only_without_server_tls() {
        assert!(ConnectFailure::Server(mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer)).server_has_no_tls());
        let refused = mysql_async::Error::Server(mysql_async::ServerError {
            code: 1045,
            message: "Access denied for user 'u'@'h' (using password: YES)".into(),
            state: "28000".into(),
        });
        assert!(!ConnectFailure::Server(refused).server_has_no_tls());
        assert!(!ConnectFailure::TimedOut.server_has_no_tls());
        let io = mysql_async::Error::Io(mysql_async::IoError::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")));
        assert!(!ConnectFailure::Server(io).server_has_no_tls());
    }

    /// A CA file makes Prefer and Require check the chain, Verify the chain
    /// and host; without one they encrypt without checking (Verify: the
    /// system's CAs). A missing file is refused, unless SSL is off.
    #[test]
    fn ca_file_verifies_the_server() {
        let dir = std::env::temp_dir().join(format!("db-mysql-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n").unwrap();
        let ca_s = ca.to_string_lossy().into_owned();
        for mode in [SslMode::Prefer, SslMode::Require] {
            let o = ssl_opts(&cfg(mode, Some(&ca_s))).unwrap().unwrap();
            assert!(!o.accept_invalid_certs() && o.skip_domain_validation() && o.disable_built_in_roots() && o.root_certs().len() == 1, "{mode:?}");
            let o = ssl_opts(&cfg(mode, None)).unwrap().unwrap();
            assert!(o.accept_invalid_certs() && o.skip_domain_validation(), "{mode:?} without a CA");
        }
        let o = ssl_opts(&cfg(SslMode::VerifyFull, Some(&ca_s))).unwrap().unwrap();
        assert!(!o.accept_invalid_certs() && !o.skip_domain_validation() && o.root_certs().len() == 1);
        let o = ssl_opts(&cfg(SslMode::VerifyFull, None)).unwrap().unwrap();
        assert!(!o.accept_invalid_certs() && !o.skip_domain_validation() && o.root_certs().is_empty());
        assert!(ssl_opts(&cfg(SslMode::Disable, Some(&ca_s))).unwrap().is_none());
        let e = ssl_opts(&cfg(SslMode::Require, Some("/nonexistent/ca.pem"))).unwrap_err();
        assert!(e.message.contains("CA certificate file not found"), "{}", e.message);
        assert!(ssl_opts(&cfg(SslMode::Disable, Some("/nonexistent/ca.pem"))).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
