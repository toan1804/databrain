//! Fast reachability check before a driver connects.
//!
//! Drivers wait 15–20 s (and cloud clients retry) before they report an
//! unreachable server. A plain DNS lookup + TCP connect tells "unknown host",
//! "connection refused" and "no answer" apart within a few seconds, with a
//! message that says what to check.

use std::time::Duration;

use databrain_connector_core::ConnectionConfig;
use tokio::net::{TcpStream, lookup_host};

use crate::EngineError;

/// The host:port the first network hop goes to (the SSH server when a
/// tunnel is used), or `None` when it can't be known up front (files,
/// Snowflake/BigQuery endpoints, Oracle connect strings, SQL Server named
/// instances found through the browser service).
pub fn target(cfg: &ConnectionConfig, default_port: Option<u16>) -> Option<(String, u16)> {
    if let Some(ssh) = &cfg.ssh {
        let h = ssh.host.trim();
        return (!h.is_empty()).then(|| (h.to_string(), ssh.port));
    }
    use databrain_connector_core::traits::NetworkTarget;
    match cfg.kind.traits().network {
        NetworkTarget::HostPort => {
            // Oracle connect strings and SQL Server named instances (found
            // through the browser service) don't say where they go.
            // A connection string/URI (Oracle, MongoDB, Neo4j) may name several hosts or an SRV record.
            if cfg.opt("connect_string").is_some() || cfg.opt("uri").is_some() || (cfg.opt("instance").is_some() && cfg.port.is_none()) {
                return None;
            }
            let host = cfg.host_or_default().trim().to_string();
            Some((host, cfg.port.or(default_port)?))
        }
        NetworkTarget::HttpsHost => {
            let h = cfg.host.as_deref()?.trim();
            let h = h.strip_prefix("https://").unwrap_or(h);
            let h = h.split('/').next().unwrap_or("").trim();
            (!h.is_empty()).then(|| (h.to_string(), 443))
        }
        NetworkTarget::Unknown => None,
    }
}

/// Resolve and open a TCP connection to `host:port` within `timeout`.
pub async fn reach(host: &str, port: u16, timeout: Duration) -> Result<(), EngineError> {
    let what = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let addrs: Vec<_> = match tokio::time::timeout(timeout, lookup_host((host, port))).await {
        Err(_) => return Err(EngineError::new("connection", format!("Could not resolve {host} within {} s (DNS or VPN problem?)", timeout.as_secs()))),
        Ok(Err(_)) => return Err(EngineError::new("connection", format!("Unknown host {host}: check the host name (or the VPN/DNS it needs)"))),
        Ok(Ok(a)) => a.collect(),
    };
    if addrs.is_empty() {
        return Err(EngineError::new("connection", format!("Unknown host {host}")));
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut refused = None;
    // Try every address (IPv6 and IPv4) in the time left.
    for a in &addrs {
        match tokio::time::timeout_at(deadline, TcpStream::connect(a)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(e)) => refused = Some(e),
            Err(_) => break,
        }
    }
    Err(match refused {
        Some(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            EngineError::new("connection", format!("Connection refused by {what}: is the server running and is the port right?"))
        }
        Some(e) if tokio::time::Instant::now() < deadline => EngineError::new("connection", format!("Cannot reach {what}: {e}")),
        _ => EngineError::new(
            "connection",
            format!("No answer from {what} within {} s: the host may be wrong, down, or behind a firewall/VPN", timeout.as_secs()),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectorKind;

    fn cfg(kind: ConnectorKind, host: &str, port: Option<u16>) -> ConnectionConfig {
        ConnectionConfig {
            kind,
            host: Some(host.into()),
            port,
            database: None,
            file_path: None,
            auth: AuthMethod::Password { user: "u".into() },
            ssl_mode: Default::default(),
            read_only: false,
            options: Default::default(),
            ssh: None,
        }
    }

    #[test]
    fn targets() {
        assert_eq!(target(&cfg(ConnectorKind::Postgres, "db", None), Some(5432)), Some(("db".into(), 5432)));
        assert_eq!(target(&cfg(ConnectorKind::Mysql, "db", Some(3307)), Some(3306)), Some(("db".into(), 3307)));
        assert_eq!(target(&cfg(ConnectorKind::Databricks, "https://adb-1.net/", None), None), Some(("adb-1.net".into(), 443)));
        assert_eq!(target(&cfg(ConnectorKind::Snowflake, "x", None), None), None);
        let mut m = cfg(ConnectorKind::Mssql, "db", None);
        m.options.insert("instance".into(), "SQLEXPRESS".into());
        assert_eq!(target(&m, Some(1433)), None, "named instance: port found by the browser service");
        let mut g = cfg(ConnectorKind::Mongodb, "", None);
        g.options.insert("uri".into(), "mongodb+srv://c.example.net".into());
        assert_eq!(target(&g, Some(27017)), None);
        assert_eq!(target(&cfg(ConnectorKind::Neo4j, "graph", None), Some(7687)), Some(("graph".into(), 7687)));
        let mut o = cfg(ConnectorKind::Oracle, "db", None);
        o.options.insert("connect_string".into(), "(DESCRIPTION=…)".into());
        assert_eq!(target(&o, Some(1521)), None);
    }

    #[tokio::test]
    async fn refused_is_fast_and_explained() {
        // A port nobody listens on: bind, read the port, close.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let t = std::time::Instant::now();
        let e = reach("127.0.0.1", port, Duration::from_secs(5)).await.unwrap_err();
        assert!(e.message.contains("refused"), "{}", e.message);
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn open_port_and_unknown_host() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        reach("127.0.0.1", port, Duration::from_secs(5)).await.unwrap();
        let e = reach("no-such-host.invalid", 5432, Duration::from_secs(5)).await.unwrap_err();
        assert!(e.message.contains("no-such-host.invalid"), "{}", e.message);
    }
}
