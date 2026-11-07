//! Endpoint resolution: scheme, host and port from the connection config.

use crate::HTTP_PORT;
use crate::HTTPS_PORT;
use databrain_connector_core::ConnectionConfig;
use databrain_connector_core::SslMode;

/// `http(s)://host:port/` for a config, and whether certificates are checked.
pub(crate) fn endpoint(cfg: &ConnectionConfig) -> (String, bool) {
    let raw = cfg.host_or_default().trim().trim_end_matches('/').to_string();
    let (forced_tls, host) = match raw.strip_prefix("https://") {
        Some(h) => (Some(true), h.to_string()),
        None => match raw.strip_prefix("http://") {
            Some(h) => (Some(false), h.to_string()),
            None => (None, raw),
        },
    };
    let tls = forced_tls.unwrap_or(match cfg.ssl_mode {
        SslMode::Disable => false,
        // "Prefer": TLS on the standard TLS ports (ClickHouse Cloud is 8443).
        SslMode::Prefer => matches!(cfg.port, Some(HTTPS_PORT) | Some(443)),
        SslMode::Require | SslMode::VerifyFull => true,
    });
    let port = cfg.port.unwrap_or(if tls { HTTPS_PORT } else { HTTP_PORT });
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
    (format!("{}://{host}:{port}/", if tls { "https" } else { "http" }), tls && cfg.ssl_mode == SslMode::Require)
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectorKind;

    #[test]
    fn endpoints() {
        let mut c = ConnectionConfig::new(ConnectorKind::Clickhouse, AuthMethod::Password { user: "u".into() });
        c.host = Some("ch.local".into());
        assert_eq!(endpoint(&c), ("http://ch.local:8123/".into(), false));
        c.port = Some(8443);
        assert_eq!(endpoint(&c).0, "https://ch.local:8443/");
        c.port = None;
        c.ssl_mode = SslMode::Require;
        assert_eq!(endpoint(&c), ("https://ch.local:8443/".into(), true));
        c.ssl_mode = SslMode::VerifyFull;
        assert_eq!(endpoint(&c), ("https://ch.local:8443/".into(), false));
        c.host = Some("https://x.clickhouse.cloud/".into());
        c.ssl_mode = SslMode::Disable;
        assert_eq!(endpoint(&c).0, "https://x.clickhouse.cloud:8443/");
        c.host = Some("::1".into());
        assert_eq!(endpoint(&c).0, "http://[::1]:8123/");
    }
}
