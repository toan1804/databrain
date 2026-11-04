use databrain_workspace::SavedQuery;
use serde_json::{Value, json};

use super::{ToolContext, ToolOutput, arg, out};
use crate::policy::Decision;
use crate::types::{AiError, Result};

impl ToolContext {
    pub(super) async fn save_query(&self, args: &Value) -> Result<ToolOutput> {
        let v = self.gate("save_query", Decision::Ask("Save query".into()), arg(args, "name")?, args).await?;
        let saved = self.engine.workspace().save_query(SavedQuery {
            id: String::new(),
            name: v.get("name").and_then(|n| n.as_str()).unwrap_or("AI query").to_string(),
            sql: v.get("sql").and_then(|n| n.as_str()).unwrap_or_default().to_string(),
            connection_id: Some(self.profile.id.clone()),
            folder_id: None,
            description: v.get("description").and_then(|d| d.as_str()).map(str::to_string),
            tags: vec!["ai".into()],
            ai_example: false,
            created_at: 0,
            updated_at: 0,
        })?;
        Ok(out(format!("Saved as \"{}\".", saved.name), json!({"saved_query": saved})))
    }

    pub(super) async fn write_editor(&self, args: &Value) -> Result<ToolOutput> {
        let sql = arg(args, "sql")?;
        let mode = args.get("mode").and_then(|m| m.as_str()).unwrap_or("replace_selection");
        let proposal = json!({"sql": sql, "mode": mode, "title": args.get("title")});
        let accepted = tokio::select! {
            a = self.host.propose_edit(&proposal) => a,
            _ = self.cancel.cancelled() => return Err(AiError::Cancelled),
        };
        self.audit("write_editor", &proposal, if accepted { "approved" } else { "denied" }, None);
        Ok(out(
            if accepted { "The user applied the SQL in the editor." } else { "The user did not apply the change." },
            json!({"proposal": proposal, "accepted": accepted}),
        ))
    }
}
