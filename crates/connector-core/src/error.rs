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
        // A statement's error without a code is the statement's (or data's)
        // words, never a sign-in's: `RAISE 'password has expired'`.
        if self.code.is_none() && self.kind == ErrorKind::Query {
            return false;
        }
        password_expired(self.code.as_deref(), &self.message)
    }
}

/// Whether a driver's `code` and `message` say the password expired:
/// Oracle ORA-28001; MySQL 1862 (refused at sign-in) and 1820 (signed in,
/// but only ALTER USER allowed); SQL Server 18487 (expired) and 18488
/// (must be changed). A number another database could also use counts only
/// with its words about a password. Without a code, the words alone (a
/// driver that keeps none), which [`ConnectorError::is_password_expired`]
/// doesn't apply to a statement's error. PostgreSQL and Redshift don't say
/// why a sign-in failed.
pub fn password_expired(code: Option<&str>, message: &str) -> bool {
    let m = message.to_lowercase();
    let says = |words: &[&str]| words.iter().any(|w| m.contains(w));
    match code {
        // Drivers add to the code: "1862 (HY000)", "18487 (line 1)".
        Some(c) => match c.split_whitespace().next().unwrap_or("") {
            "ORA-28001" => true,
            // MySQL "You must reset your password…", MariaDB "You must SET PASSWORD before…".
            "1862" | "1820" => says(&["password has expired", "must reset your password", "must set password"]),
            "18487" | "18488" => says(&["password of the account has expired", "password of the account must be changed"]),
            _ => false,
        },
        None => says(&["ora-28001", "password has expired", "password of the account has expired", "password of the account must be changed"]),
    }
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
            (None, "Login failed for user 'app'. Reason: The password of the account has expired."),
            (Some("1862 (HY000)"), "Your password has expired. To log in you must change it using a client that supports expired passwords."),
            (Some("1820 (HY000)"), "You must reset your password using ALTER USER statement before executing this statement."),
            (Some("1820 (HY000)"), "You must SET PASSWORD before executing this statement"),
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
            // Another database's 1820 or 18487, and an Oracle code without its prefix.
            (Some("1820 (line 3)"), "CREATE DATABASE failed. Some file names listed could not be created."),
            (Some("18487"), "something else"),
            (Some("28001"), "x"),
            (Some("42P01"), "relation \"password has expired\" does not exist"),
        ] {
            assert!(!password_expired(code, msg), "{code:?} {msg}");
        }
        assert!(ConnectorError::connection("ORA-28001: the password has expired").with_code("ORA-28001").is_password_expired());
        // A statement's own words (a RAISE, a cast of data) never are.
        assert!(!ConnectorError::query("invalid input syntax for type integer: \"your password has expired\"").is_password_expired());
        assert!(!ConnectorError::query("Your password has expired").with_code("P0001").is_password_expired());
        assert!(ConnectorError::connection("ORA-28001: the password has expired").is_password_expired());
    }
}
