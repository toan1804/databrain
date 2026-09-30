//! DataBrain AI mode: LLM providers, database knowledge, tools with a policy
//! engine, and the agent loop.

pub mod agent;
pub mod kiro;
pub mod knowledge;
pub mod mcp;
pub mod policy;
pub mod providers;
pub mod resultsql;
pub mod sse;
pub mod tools;
pub mod types;

pub use agent::{Agent, AgentEvent, AgentRequest, AgentSink, Mode, UiContext, provider_for};
pub use providers::{LlmProvider, ProviderAuth, ProviderConfig, ProviderKind};
pub use tools::{Caller, ToolContext, ToolHost};
pub use types::*;
