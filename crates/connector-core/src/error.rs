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
        Self {
            kind,
            message: message.into(),
            code: None,
            position: None,
        }
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
