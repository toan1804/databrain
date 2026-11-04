//! Mapping of driver errors to connector errors.

use databrain_connector_core::ConnectorError;
use databrain_connector_core::ErrorKind;

pub(crate) fn map_err(e: oracle::Error) -> ConnectorError {
    if let Some(db) = e.db_error() {
        let code = db.code();
        if code == 1013 {
            // ORA-01013: user requested cancel of current operation
            return ConnectorError::cancelled();
        }
        let kind = if matches!(code, 1017 | 12154 | 12514 | 12541 | 12545 | 12170 | 3113 | 3114 | 3135) { ErrorKind::Connection } else { ErrorKind::Query };
        let offset = db.offset();
        let mut ce = ConnectorError::new(kind, db.message().to_string()).with_code(format!("ORA-{code:05}"));
        if offset > 0 {
            ce = ce.with_position(Some(offset + 1));
        }
        return ce;
    }
    let msg = e.to_string();
    if msg.contains("DPI-1047") {
        // `oracle_client_missing` lets the UI offer the one-click install.
        return ConnectorError::connection(format!(
            "Oracle Instant Client was not found (DPI-1047). Install it from the connection dialog (“Install Instant Client”), or set the connection's “Instant Client directory”.\n\n{msg}"
        ))
        .with_code("oracle_client_missing");
    }
    ConnectorError::query(msg)
}
