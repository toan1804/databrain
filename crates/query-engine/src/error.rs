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

impl From<ConnectorError> for EngineError {
    fn from(e: ConnectorError) -> Self {
        let kind = serde_json::to_value(e.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "internal".into());
        Self { kind, message: e.message, code: e.code, position: e.position }
    }
}

impl From<databrain_workspace::Error> for EngineError {
    fn from(e: databrain_workspace::Error) -> Self {
        let kind = match e {
            databrain_workspace::Error::NotFound(_) => "not_found",
            databrain_workspace::Error::Invalid(_) => "invalid",
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
