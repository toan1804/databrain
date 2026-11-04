use databrain_connector_core::{ConnState, ConnectionConfig, ConnectorError, Result, SslMode};
use postgres_native_tls::MakeTlsConnector;
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::Client;

use crate::error::map_err;

pub(crate) fn tls_connector(mode: SslMode) -> Result<MakeTlsConnector> {
    let mut b = native_tls::TlsConnector::builder();
    // libpq semantics: prefer/require encrypt but do not verify the server
    // certificate; verify-full verifies chain and host name.
    if mode != SslMode::VerifyFull {
        b.danger_accept_invalid_certs(true);
        b.danger_accept_invalid_hostnames(true);
    }
    let c = b.build().map_err(|e| ConnectorError::config(format!("TLS setup failed: {e}")))?;
    Ok(MakeTlsConnector::new(c))
}

/// Amazon Redshift (the connection's "Database type", or its endpoint host):
/// port 5439, no startup options, IAM sign-in through the AWS CLI.
pub(crate) fn redshift_flavor(cfg: &ConnectionConfig) -> bool {
    cfg.is_redshift()
}

/// Session setup sent as SQL where startup options can't be (Redshift).
pub(crate) const REDSHIFT_INIT: &str = "SET DateStyle TO ISO, MDY";

/// Connect, run the connection's I/O task and the setup SQL (`init`).
/// `temporary`: refused sign-ins are auth errors (new credentials may work).
pub(crate) async fn open_link(
    pg: &tokio_postgres::Config,
    tls: MakeTlsConnector,
    init: Option<&str>,
    temporary: bool,
) -> Result<(Client, tokio::task::AbortHandle)> {
    let (client, connection) = pg.connect(tls).await.map_err(|e| {
        let mut ce = map_err(&e);
        let refused = e.as_db_error().is_some_and(|d| d.code().code().starts_with("28"));
        ce.kind = if refused && temporary { databrain_connector_core::ErrorKind::Auth } else { databrain_connector_core::ErrorKind::Connection };
        ce
    })?;
    // Ends when the client is dropped, the socket closes, or it is aborted.
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    if let Some(sql) = init
        && let Err(e) = client.batch_execute(sql).await
    {
        task.abort();
        return Err(map_err(&e));
    }
    Ok((client, task.abort_handle()))
}

/// The session's connection, replaceable: a result cut at the row limit
/// would otherwise keep arriving (the connection reads and discards it up
/// to the end before the next statement).
pub(crate) struct Link {
    pub(crate) pg: tokio_postgres::Config,
    pub(crate) tls: MakeTlsConnector,
    /// Setup SQL run on every new connection (Redshift: DateStyle).
    pub(crate) init: Option<String>,
    pub(crate) temporary: bool,
    pub(crate) current: std::sync::RwLock<(Arc<Client>, tokio::task::AbortHandle)>,
    /// What the connection carries (transactions, SET, temp tables).
    pub(crate) state: ConnState,
}

impl Link {
    pub(crate) fn client(&self) -> Arc<Client> {
        self.current.read().unwrap_or_else(|e| e.into_inner()).0.clone()
    }

    /// Put a new connection in place of `old` and close `old` (the server
    /// stops sending when its socket goes away). False when no new
    /// connection could be opened; nothing changes then.
    pub(crate) async fn replace(&self, old: &Arc<Client>) -> bool {
        let Ok(Ok((client, task))) =
            tokio::time::timeout(Duration::from_secs(15), open_link(&self.pg, self.tls.clone(), self.init.as_deref(), self.temporary)).await
        else {
            return false;
        };
        let mut cur = self.current.write().unwrap_or_else(|e| e.into_inner());
        if !Arc::ptr_eq(&cur.0, old) {
            // Replaced meanwhile (can't happen: one statement at a time).
            task.abort();
            return true;
        }
        let (_, old_task) = std::mem::replace(&mut *cur, (Arc::new(client), task));
        old_task.abort();
        self.state.reset();
        true
    }
}
