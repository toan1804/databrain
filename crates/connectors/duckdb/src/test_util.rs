use std::sync::Arc;

use databrain_auth::{AuthMethod, InlineCredentialSource};
use databrain_connector_core::{ConnectionConfig, Connector, ExecOptions, Session};

use crate::DuckdbConnector;

pub(crate) async fn session(cfg: ConnectionConfig) -> Box<dyn Session> {
    DuckdbConnector::new().connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::None, None))).await.unwrap()
}

pub(crate) async fn q(s: &dyn Session, sql: &str) -> databrain_connector_core::Collected {
    s.execute(sql, ExecOptions::default()).await.unwrap().collect().await.unwrap()
}
