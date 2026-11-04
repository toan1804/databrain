//! Connecting: driver choice, connect string and opening a session.

use crate::agent;
use crate::client;
use crate::error::map_err;
use crate::exec::blocking;
use crate::session::Backend;
use crate::session::OracleSession;
use databrain_auth::Credential;
use databrain_auth::CredentialSource;
use databrain_connector_core::ConnectionConfig;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::Result;
use oracle::Connection;
use secrecy::ExposeSecret;
use std::sync::Arc;
use std::sync::Mutex;

pub(crate) fn init_client(lib_dir: Option<&str>) -> Result<()> {
    // Retries on every connect until the library loads (e.g. right after
    // installing Instant Client), using the folder found automatically.
    // A checked folder whose libraries changed is refused outright: the
    // driver would otherwise load Instant Client from the search path.
    match client::init_client(lib_dir, &[]) {
        Err(client::InitError::Changed(m)) => Err(ConnectorError::config(m)),
        _ => Ok(()),
    }
}

pub(crate) fn connect_string(cfg: &ConnectionConfig) -> String {
    if let Some(cs) = cfg.opt("connect_string") {
        return cs.to_string();
    }
    let service = cfg.opt("service_name").or(cfg.database.as_deref().filter(|d| !d.is_empty())).unwrap_or("FREEPDB1");
    format!("{}:{}/{}", cfg.host_or_default(), cfg.port.unwrap_or(1521), service)
}

pub(crate) async fn open(cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<OracleSession> {
    {
        let (user, password) = match creds.get().await? {
            Credential::Password { user, password } => (user, password.expose_secret().to_string()),
            _ => return Err(ConnectorError::config("Oracle requires user/password authentication")),
        };
        if !uses_instant_client(cfg) {
            let path = agent::ensure().await?;
            let a = agent::Agent::spawn(&path).await?;
            let options: serde_json::Map<String, serde_json::Value> =
                parse_driver_options(cfg.opt("driver_options").unwrap_or("")).into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
            a.call(serde_json::json!({
                "op": "connect",
                "user": user,
                "password": password,
                "host": cfg.host_or_default(),
                "port": cfg.port.unwrap_or(1521),
                "service": cfg.opt("service_name").or(cfg.database.as_deref().filter(|d| !d.is_empty())).unwrap_or("FREEPDB1"),
                "connect_string": cfg.opt("connect_string").unwrap_or(""),
                "options": options,
                "read_only": cfg.read_only,
            }))
            .await?;
            return Ok(OracleSession { backend: Backend::Thin(a) });
        }
        let cs = connect_string(cfg);
        let lib = cfg.opt("client_lib_dir").map(str::to_string);
        let read_only = cfg.read_only;
        let conn = blocking(move || {
            init_client(lib.as_deref())?;
            let mut c = Connection::connect(&user, &password, &cs).map_err(map_err)?;
            // Read-only: one READ ONLY transaction, never committed (with
            // autocommit on, it would end right after it starts).
            c.set_autocommit(!read_only);
            if read_only {
                c.execute("set transaction read only", &[]).map_err(map_err)?;
            }
            let _ = c.execute("alter session set nls_date_format = 'YYYY-MM-DD HH24:MI:SS'", &[]);
            Ok(c)
        })
        .await?;
        Ok(OracleSession { backend: Backend::Thick { conn: Arc::new(conn), lock: Arc::new(Mutex::new(())) } })
    }
}

/// Instant Client (thick) driver chosen for this connection?
pub fn uses_instant_client(cfg: &ConnectionConfig) -> bool {
    matches!(cfg.opt("driver").map(str::trim), Some("instant_client" | "thick"))
}

/// `KEY=value; KEY=value` (also newlines) → pairs.
pub(crate) fn parse_driver_options(s: &str) -> Vec<(String, String)> {
    s.split([';', '\n'])
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectorKind;

    #[test]
    fn builds_connect_string() {
        let mut c = ConnectionConfig::new(ConnectorKind::Oracle, AuthMethod::Password { user: "u".into() });
        c.host = Some("db".into());
        c.options.insert("service_name".into(), "ORCLPDB1".into());
        assert_eq!(connect_string(&c), "db:1521/ORCLPDB1");
        c.options.insert("connect_string".into(), "prod_tns".into());
        assert_eq!(connect_string(&c), "prod_tns");
    }
}
