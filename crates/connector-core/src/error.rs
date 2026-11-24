use serde::Serialize;

pub type Result<T, E = ConnectorError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Auth,
    Connection,
    Query,
    Cancelled,
    Unsupported,
    Config,
    Internal,
}

/// Error returned by connectors. Keeps the vendor error code and, when the
/// server reports it, the 1-based character position of the error in the SQL.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct ConnectorError {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
}

impl ConnectorError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), code: None, position: None }
    }
    pub fn query(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Query, message)
    }
    pub fn connection(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Connection, message)
    }
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "Query cancelled")
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    pub fn with_position(mut self, position: Option<u32>) -> Self {
        self.position = position;
        self
    }
    pub fn is_cancelled(&self) -> bool {
        self.kind == ErrorKind::Cancelled
    }

    /// The server refused the sign-in (or every statement) because the
    /// password expired, so it must be changed before anything else.
    pub fn is_password_expired(&self) -> bool {
        password_expired(self.code.as_deref(), &self.message)
    }
}

/// Whether a driver's `code` and `message` say the password expired:
/// Oracle ORA-28001; MySQL 1862 (refused at sign-in) and 1820 (signed in,
/// but only ALTER USER allowed); SQL Server 18487 (expired) and 18488
/// (must be changed). Their English messages too, for a driver that keeps
/// no code. PostgreSQL and Redshift don't say why a sign-in failed.
pub fn password_expired(code: Option<&str>, message: &str) -> bool {
    // Drivers add to the code: "1862 (HY000)", "18487 (line 1)".
    let code = code.and_then(|c| c.split_whitespace().next()).unwrap_or("");
    let m = message.to_lowercase();
    matches!(code, "ORA-28001" | "28001" | "1862" | "1820" | "18487" | "18488")
        || m.contains("ora-28001")
        || m.contains("password has expired")
        || m.contains("password of the account has expired")
        || m.contains("password of the account must be changed")
        || m.contains("must reset your password")
}

impl From<databrain_auth::AuthError> for ConnectorError {
    fn from(e: databrain_auth::AuthError) -> Self {
        Self::new(ErrorKind::Auth, e.to_string())
    }
}

impl From<arrow::error::ArrowError> for ConnectorError {
    fn from(e: arrow::error::ArrowError) -> Self {
        Self::internal(format!("arrow: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_passwords_by_code_and_message() {
        for (code, msg) in [
            (Some("ORA-28001"), "ORA-28001: the password has expired"),
            (None, "ORA-28001: the password has expired"),
            (Some("1862 (HY000)"), "Your password has expired. To log in you must change it using a client that supports expired passwords."),
            (Some("1820 (HY000)"), "You must reset your password using ALTER USER statement before executing this statement."),
            (Some("18487 (line 1)"), "Login failed for user 'app'. Reason: The password of the account has expired."),
            (Some("18488 (line 1)"), "Login failed for user 'app'. Reason: The password of the account must be changed."),
        ] {
            assert!(password_expired(code, msg), "{code:?} {msg}");
        }
        // A wrong password, "will expire soon" (signed in, a warning) and an unrelated error aren't.
        for (code, msg) in [
            (Some("ORA-01017"), "ORA-01017: invalid username/password; logon denied"),
            (Some("ORA-28002"), "ORA-28002: the password will expire within 7 days"),
            (Some("28P01"), "password authentication failed for user \"app\""),
            (Some("18456 (line 1)"), "Login failed for user 'app'."),
            (Some("18487000"), "x"),
            (None, "table passwords has expired rows"),
        ] {
            assert!(!password_expired(code, msg), "{code:?} {msg}");
        }
        assert!(ConnectorError::connection("ORA-28001: the password has expired").with_code("ORA-28001").is_password_expired());
    }
}
