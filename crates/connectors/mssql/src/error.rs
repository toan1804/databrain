//! Mapping of driver errors to connector errors.

use databrain_connector_core::ConnectorError;

pub(crate) fn map_err(e: tiberius::error::Error) -> ConnectorError {
    match e {
        tiberius::error::Error::Server(t) => ConnectorError::query(t.message().to_string()).with_code(format!("{} (line {})", t.code(), t.line())),
        tiberius::error::Error::Io { message, .. } => ConnectorError::connection(message),
        tiberius::error::Error::Routing { host, port } => ConnectorError::connection(format!("server redirected to {host}:{port}; connect to that address")),
        tiberius::error::Error::Tls(m) => ConnectorError::connection(tls_hint(&m)),
        other => ConnectorError::query(other.to_string()),
    }
}

/// TLS errors with what to change (rustls speaks TLS 1.2 and 1.3 only).
pub(crate) fn tls_hint(m: &str) -> String {
    let l = m.to_ascii_lowercase();
    let hint = if l.contains("protocolversion") || l.contains("peerincompatible") || l.contains("handshakefailure") {
        "The server may only offer TLS 1.0/1.1 (SQL Server 2014 and older without the TLS 1.2 update); update it, or set SSL to Disable."
    } else if l.contains("certificate") || l.contains("unknownissuer") || l.contains("notvalidforname") {
        "The server's certificate is not trusted: turn on \"Trust server certificate\" or use SSL Prefer/Require."
    } else {
        ""
    };
    if hint.is_empty() { format!("TLS error: {m}") } else { format!("TLS error: {m}. {hint}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_errors_say_what_to_change() {
        assert!(tls_hint("received fatal alert: ProtocolVersion").contains("TLS 1.0/1.1"));
        assert!(tls_hint("invalid peer certificate: UnknownIssuer").contains("Trust server certificate"));
        assert_eq!(tls_hint("eof"), "TLS error: eof");
    }
}
