//! Connection options: TLS, init commands and connecting with a timeout.

use crate::error::map_err;
use databrain_connector_core::ConnectionConfig;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ErrorKind;
use databrain_connector_core::Result;
use databrain_connector_core::SslMode;
use mysql_async::Conn;
use mysql_async::Opts;
use mysql_async::OptsBuilder;
use mysql_async::SslOpts;
use std::time::Duration;

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

pub(crate) fn ssl_opts(mode: SslMode) -> Option<SslOpts> {
    match mode {
        SslMode::Disable => None,
        SslMode::Prefer | SslMode::Require => Some(SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)),
        SslMode::VerifyFull => Some(SslOpts::default()),
    }
}

pub(crate) async fn connect_with_timeout(opts: Opts) -> Result<Conn> {
    match tokio::time::timeout(Duration::from_secs(15), Conn::new(opts)).await {
        Ok(r) => r.map_err(|e| {
            let mut ce = map_err(e);
            if ce.kind == ErrorKind::Query {
                ce.kind = ErrorKind::Connection;
            }
            ce
        }),
        Err(_) => Err(ConnectorError::connection("connection timed out")),
    }
}
