use databrain_connector_core::ConnectorError;
use tokio_postgres::error::ErrorPosition;

pub(crate) fn map_err(e: &tokio_postgres::Error) -> ConnectorError {
    if let Some(db) = e.as_db_error() {
        let mut msg = db.message().to_string();
        if let Some(d) = db.detail() {
            msg.push_str("\nDETAIL: ");
            msg.push_str(d);
        }
        if let Some(h) = db.hint() {
            msg.push_str("\nHINT: ");
            msg.push_str(h);
        }
        let pos = match db.position() {
            Some(ErrorPosition::Original(p)) => Some(*p),
            _ => None,
        };
        let code = db.code().code();
        if code == "57014" {
            return ConnectorError::cancelled();
        }
        return ConnectorError::query(msg).with_code(code).with_position(pos);
    }
    if e.is_closed() {
        return ConnectorError::connection("connection closed");
    }
    // Include the source chain (e.g. "connection refused").
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    ConnectorError::connection(msg)
}
