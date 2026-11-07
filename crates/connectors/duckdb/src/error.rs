use databrain_connector_core::ConnectorError;

pub(crate) fn map_err(e: duckdb::Error) -> ConnectorError {
    let msg = e.to_string();
    if msg.contains("INTERRUPT") || msg.contains("Interrupted") {
        return ConnectorError::cancelled();
    }
    ConnectorError::query(msg)
}

/// What a sealed session refused, and how the user allows it.
pub(crate) fn sandbox_hint(mut e: ConnectorError) -> ConnectorError {
    let m = e.message.to_ascii_lowercase();
    if m.contains("disabled through configuration")
        || m.contains("permission error")
        || m.contains("lock_configuration")
        || m.contains("configuration has been locked")
    {
        e.message = format!(
            "{} (SQL from the assistant runs in a DuckDB sandbox without file or network access and can't change settings; allow it in the connection's AI settings)",
            e.message
        );
    }
    e
}
