//! Errors the engine reports to the UI (a kind, a message, a position).

use super::*;

// ------------------------------------------------------------------ errors

/// Serializable error returned to the UI.
#[derive(Debug, Clone, Serialize, thiserror::Error, ts_rs::TS)]
#[ts(export)]
#[error("{message}")]
pub struct EngineError {
    /// Machine-readable category: `auth`, `connection`, `query`, `cancelled`,
    /// `unsupported`, `config`, `internal`, `not_found`, `invalid`, `busy`.
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub code: Option<String>,
    /// 1-based character position of the error within the statement.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub position: Option<u32>,
}

impl EngineError {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        Self { kind: kind.into(), message: message.into(), code: None, position: None }
    }
}

/// [`EngineError::code`] of a refusal because the connection's password
/// expired ([`ConnectorError::is_password_expired`]).
pub const PASSWORD_EXPIRED: &str = "password_expired";

impl From<ConnectorError> for EngineError {
    fn from(e: ConnectorError) -> Self {
        // An expired password is said apart (the app offers to change it),
        // whatever kind the driver gave it. Its own code isn't kept (Oracle's
        // is in the message; MySQL's and SQL Server's aren't).
        if e.is_password_expired() {
            return Self { kind: "auth".into(), message: e.message, code: Some(PASSWORD_EXPIRED.into()), position: None };
        }
        let kind = serde_json::to_value(e.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "internal".into());
        Self { kind, message: e.message, code: e.code, position: e.position }
    }
}

impl From<databrain_workspace::Error> for EngineError {
    fn from(e: databrain_workspace::Error) -> Self {
        let kind = match e {
            databrain_workspace::Error::NotFound(_) => "not_found",
            databrain_workspace::Error::Invalid(_) => "invalid",
            databrain_workspace::Error::Conflict(_) => "conflict",
            _ => "internal",
        };
        Self::new(kind, e.to_string())
    }
}

impl From<databrain_result_store::Error> for EngineError {
    fn from(e: databrain_result_store::Error) -> Self {
        let kind = match e {
            databrain_result_store::Error::NotFound(_) => "not_found",
            databrain_result_store::Error::InvalidColumn(_) | databrain_result_store::Error::InvalidFilterValue(_) => "invalid",
            _ => "internal",
        };
        Self::new(kind, e.to_string())
    }
}

impl From<databrain_auth::AuthError> for EngineError {
    fn from(e: databrain_auth::AuthError) -> Self {
        match e {
            databrain_auth::AuthError::ReauthRequired => Self::new("reauth_required", "Sign in to this connection first"),
            databrain_auth::AuthError::Cancelled => Self::new("cancelled", "Sign-in cancelled"),
            other => Self::new("auth", other.to_string()),
        }
    }
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

#[cfg(test)]
mod password_tests {
    use super::*;

    /// An expired password is said apart (code `password_expired`, kind
    /// `auth`), whatever kind the driver gave; other errors keep theirs.
    #[test]
    fn expired_passwords_are_told_apart() {
        let e = EngineError::from(ConnectorError::connection("ORA-28001: the password has expired").with_code("ORA-28001"));
        assert_eq!((e.kind.as_str(), e.code.as_deref()), ("auth", Some(PASSWORD_EXPIRED)));
        assert!(e.message.contains("ORA-28001"));
        let e = EngineError::from(
            ConnectorError::query("You must reset your password using ALTER USER statement before executing this statement.").with_code("1820 (HY000)"),
        );
        assert_eq!(e.code.as_deref(), Some(PASSWORD_EXPIRED));
        // Data or a RAISE saying so is the statement's error, its position kept.
        let e = EngineError::from(ConnectorError::query("invalid input syntax for type integer: \"your password has expired\"").with_position(Some(8)));
        assert_eq!((e.kind.as_str(), e.code, e.position), ("query", None, Some(8)));
        let e = EngineError::from(ConnectorError::connection("ORA-01017: invalid username/password").with_code("ORA-01017"));
        assert_eq!((e.kind.as_str(), e.code.as_deref()), ("connection", Some("ORA-01017")));
    }
}
