//! Mapping of network and server failures to connector errors.

use databrain_connector_core::ConnectorError;
use databrain_connector_core::ErrorKind;

/// The result broke off after it started: ClickHouse does that when the
/// query fails midway, and its message doesn't always come with it.
pub(crate) fn ended_early(e: reqwest::Error, query_id: &str, rows: usize) -> ConnectorError {
    let mut err = net_err(e);
    err.message = format!(
        "ClickHouse stopped sending the result after {rows} rows, usually because the query failed on the server; its error is in system.query_log (query_id = '{query_id}'). ({})",
        err.message
    );
    err
}

pub(crate) fn net_err(e: reqwest::Error) -> ConnectorError {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        let t = s.to_string();
        if !msg.contains(&t) {
            msg.push_str(": ");
            msg.push_str(&t);
        }
        src = s.source();
    }
    ConnectorError::connection(msg)
}

/// `Code: 60. DB::Exception: Table x doesn't exist. (UNKNOWN_TABLE) (version 24.3.1)` → error.
pub(crate) fn server_error(code: Option<u32>, status: u16, text: &str) -> ConnectorError {
    let text = text.trim();
    let code = code.or_else(|| text.strip_prefix("Code: ").and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok()));
    let mut msg = text;
    if let Some(i) = msg.find("DB::Exception: ") {
        msg = &msg[i + "DB::Exception: ".len()..];
    }
    if let Some(i) = msg.rfind(" (version ") {
        msg = &msg[..i];
    }
    // The error name: `(UNKNOWN_TABLE)` at the end.
    let name = msg
        .strip_suffix(')')
        .and_then(|m| m.rfind(" (").map(|i| (&m[i + 2..], &m[..i])))
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()));
    let (name, msg) = match name {
        Some((n, m)) => (Some(n.to_string()), m.trim_end_matches('.').to_string() + "."),
        None => (None, msg.to_string()),
    };
    let kind = match (code, status) {
        (Some(394), _) => return ConnectorError::cancelled(),
        (Some(516 | 192 | 193 | 194 | 195 | 497), _) | (_, 401 | 403) => ErrorKind::Auth,
        (None, 502..=504) => ErrorKind::Connection,
        _ => ErrorKind::Query,
    };
    let mut e = ConnectorError::new(kind, if msg.is_empty() { format!("ClickHouse error (HTTP {status})") } else { msg.clone() });
    if let Some(c) = code {
        e = e.with_code(match name {
            Some(n) => format!("{c} ({n})"),
            None => c.to_string(),
        });
    }
    // Syntax errors: "failed at position 8 (…)" (1-based).
    if let Some(p) = msg.split("failed at position ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse::<u32>().ok()) {
        e = e.with_position(Some(p));
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors() {
        let e = server_error(Some(60), 404, "Code: 60. DB::Exception: Table default.x does not exist. (UNKNOWN_TABLE) (version 24.3.2.23 (official build))\n");
        assert_eq!((e.kind, e.message.as_str(), e.code.as_deref()), (ErrorKind::Query, "Table default.x does not exist.", Some("60 (UNKNOWN_TABLE)")));
        let e = server_error(
            None,
            400,
            "Code: 62. DB::Exception: Syntax error: failed at position 8 ('fro'): fro x. Expected one of: ... (SYNTAX_ERROR) (version 24.3)",
        );
        assert_eq!((e.position, e.code.as_deref()), (Some(8), Some("62 (SYNTAX_ERROR)")));
        assert_eq!(server_error(Some(516), 403, "Code: 516. DB::Exception: default: Authentication failed. (AUTHENTICATION_FAILED)").kind, ErrorKind::Auth);
        assert_eq!(server_error(Some(394), 500, "Code: 394. DB::Exception: Query was cancelled. (QUERY_WAS_CANCELLED)").kind, ErrorKind::Cancelled);
    }
}
