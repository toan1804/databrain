//! Opening a connection: TCP, SQL Browser and login.

use crate::error::map_err;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::Result;
use std::time::Duration;
use tiberius::Client;
use tiberius::Config;
use tiberius::SqlBrowser;
use tokio::net::TcpStream;
use tokio_util::compat::Compat;
use tokio_util::compat::TokioAsyncWriteCompatExt;

pub(crate) type Conn = Client<Compat<TcpStream>>;

pub(crate) async fn open(c: &Config, use_browser: bool) -> Result<Conn> {
    let fut = async {
        let tcp = if use_browser {
            TcpStream::connect_named(c).await.map_err(map_err)?
        } else {
            TcpStream::connect(c.get_addr()).await.map_err(|e| ConnectorError::connection(e.to_string()))?
        };
        tcp.set_nodelay(true).ok();
        Client::connect(c.clone(), tcp.compat_write()).await.map_err(|e| {
            let mut e = map_err(e);
            e.kind = databrain_connector_core::ErrorKind::Connection;
            e
        })
    };
    tokio::time::timeout(Duration::from_secs(20), fut).await.map_err(|_| ConnectorError::connection("connection timed out"))?
}
