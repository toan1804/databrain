//! Provider-neutral chat types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// Result of a tool call (sent back to the model).
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON object.
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub text: String,
    /// Tool calls requested by the assistant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For `Role::Tool`: the call this result answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into(), tool_calls: vec![], tool_call_id: None, tool_name: None }
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, text: text.into(), tool_calls, tool_call_id: None, tool_name: None }
    }
    pub fn tool(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            text: content.into(),
            tool_calls: vec![],
            tool_call_id: Some(call.id.clone()),
            tool_name: Some(call.name.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    TextDelta(String),
    /// A complete tool call (adapters assemble streamed argument deltas).
    ToolCall(ToolCall),
    Usage { input: u32, output: u32 },
    Done { stop_reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum AiError {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Auth(String),
    #[error("{0}")]
    Provider(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("stopped")]
    Cancelled,
    #[error("{0}")]
    Policy(String),
    #[error("{0}")]
    Internal(String),
}

impl From<databrain_auth::AuthError> for AiError {
    fn from(e: databrain_auth::AuthError) -> Self {
        match e {
            databrain_auth::AuthError::Cancelled => AiError::Cancelled,
            other => AiError::Auth(other.to_string()),
        }
    }
}

impl From<databrain_workspace::Error> for AiError {
    fn from(e: databrain_workspace::Error) -> Self {
        AiError::Internal(e.to_string())
    }
}

impl From<databrain_query_engine::EngineError> for AiError {
    fn from(e: databrain_query_engine::EngineError) -> Self {
        AiError::Internal(e.message)
    }
}

pub type Result<T, E = AiError> = std::result::Result<T, E>;
