//! Mapping of driver errors to connector errors.

use databrain_connector_core::ConnectorError;

pub(crate) fn map_err(e: tiberius::error::Error) -> ConnectorError {
    match e {
        tiberius::error::Error::Server(t) => ConnectorError::query(t.message().to_string()).with_code(format!("{} (line {})", t.code(), t.line())),
        tiberius::error::Error::Io { message, .. } => ConnectorError::connection(message),
        tiberius::error::Error::Routing { host, port } => ConnectorError::connection(format!("server redirected to {host}:{port}; connect to that address")),
        tiberius::error::Error::Tls(m) => ConnectorError::connection(tls_hint(&m)),
        // The server can't encrypt (prelogin): SSL Prefer then connects
        // without (`tls_unavailable`); Require and Verify stop here.
        tiberius::error::Error::Protocol(m) if m.contains("does not allow the requested encryption level") => {
            ConnectorError::connection(format!("{m} The server doesn't offer encryption: set SSL to Prefer or Disable to connect without it."))
                .with_code(TLS_UNAVAILABLE)
        }
        other => ConnectorError::query(other.to_string()),
    }
}

/// The code of a connection refused because the server offers no encryption.
pub(crate) const TLS_UNAVAILABLE: &str = "tls_unavailable";

/// Whether a failed connection may be tried again without encryption: only
/// for SSL Prefer, and only when the server said it can't encrypt (not for
/// a certificate, handshake or network failure).
pub(crate) fn prefer_falls_back(mode: databrain_connector_core::SslMode, e: &ConnectorError) -> bool {
    mode == databrain_connector_core::SslMode::Prefer && e.code.as_deref() == Some(TLS_UNAVAILABLE)
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

    /// The server can't encrypt: Prefer connects without, Require and
    /// Verify refuse; other failures never fall back.
    #[test]
    fn only_prefer_falls_back_and_only_when_the_server_cant_encrypt() {
        use databrain_connector_core::SslMode;
        let no_tls = map_err(tiberius::error::Error::Protocol("Server does not allow the requested encryption level.".into()));
        assert_eq!(no_tls.code.as_deref(), Some(TLS_UNAVAILABLE));
        assert!(prefer_falls_back(SslMode::Prefer, &no_tls));
        assert!(!prefer_falls_back(SslMode::Require, &no_tls));
        assert!(!prefer_falls_back(SslMode::VerifyFull, &no_tls));
        assert!(!prefer_falls_back(SslMode::Disable, &no_tls));
        for other in
            [tiberius::error::Error::Tls("invalid peer certificate: UnknownIssuer".into()), tiberius::error::Error::Protocol("unexpected token".into())]
        {
            assert!(!prefer_falls_back(SslMode::Prefer, &map_err(other)));
        }
    }
}
