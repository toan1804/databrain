use serde_json::Value;

use super::{Caller, MCP_ASK_HELP, ToolContext};
use crate::policy::Decision;
use crate::types::{AiError, Result};

impl ToolContext {
    pub(super) async fn gate(&self, tool: &str, decision: Decision, summary: &str, detail: &Value) -> Result<Value> {
        match decision {
            Decision::Allow => {
                self.audit(tool, detail, "allowed", None);
                Ok(detail.clone())
            }
            Decision::Deny(reason) => Err(AiError::Policy(reason)),
            Decision::Confirm(reason) if self.caller == Caller::Mcp => {
                self.audit(tool, detail, "blocked", Some(&reason));
                Err(AiError::Policy(format!("{reason}. Only the user can approve this, in DataBrain's assistant; an MCP client can't")))
            }
            Decision::Ask(reason) | Decision::Confirm(reason) => {
                if self.caller == Caller::Mcp && !self.host.can_ask() {
                    // The client can't show DataBrain's question. Its own tool
                    // approval is not the same thing (many clients auto-approve
                    // tools), so it counts only where the connection says so.
                    if self.profile.ai_policy.mcp_client_approves {
                        self.audit(tool, detail, "approved by MCP client", Some(&reason));
                        return Ok(detail.clone());
                    }
                    self.audit(tool, detail, "blocked", Some(&reason));
                    return Err(AiError::Policy(format!(
                        "{reason}. DataBrain asks the user before this, and {}. {MCP_ASK_HELP}",
                        self.host.cannot_ask_reason()
                    )));
                }
                // Stop ends the wait for the user's answer at once.
                let question = format!("{reason}. {summary}");
                let answer = tokio::select! {
                    a = self.host.approve(tool, &question, detail) => a,
                    _ = self.cancel.cancelled() => {
                        self.audit(tool, detail, "denied", Some("stopped"));
                        return Err(AiError::Cancelled);
                    }
                };
                match answer {
                    Some(v) => {
                        self.audit(tool, &v, "approved", None);
                        Ok(v)
                    }
                    // The question went out but no answer came back (the client
                    // didn't show it, or went away): say so, not "declined".
                    None if self.caller == Caller::Mcp && !self.host.can_ask() => {
                        self.audit(tool, detail, "denied", Some("no answer"));
                        Err(AiError::Policy(format!(
                            "{reason}. DataBrain asked the user through this MCP client, but {}. {MCP_ASK_HELP}",
                            self.host.cannot_ask_reason()
                        )))
                    }
                    None => {
                        self.audit(tool, detail, "denied", None);
                        Err(AiError::Policy("the user declined".into()))
                    }
                }
            }
        }
    }
}
