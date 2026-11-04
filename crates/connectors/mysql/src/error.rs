//! Mapping of driver errors to connector errors.

use databrain_connector_core::ConnectorError;

pub(crate) fn map_err(e: mysql_async::Error) -> ConnectorError {
    match e {
        mysql_async::Error::Server(se) => {
            if se.code == 1317 {
                // ER_QUERY_INTERRUPTED
                return ConnectorError::cancelled();
            }
            ConnectorError::query(se.message).with_code(format!("{} ({})", se.code, se.state))
        }
        mysql_async::Error::Io(io) => ConnectorError::connection(io.to_string()),
        other => ConnectorError::query(other.to_string()),
    }
}
