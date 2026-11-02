//! Inline ("ghost") completion in the SQL editor: one short, non-agent
//! model call that continues the SQL at the cursor. Uses the provider's fast
//! model, the same per-connection AI policy as the assistant, and a few
//! relevant tables from the knowledge index as context.

use std::sync::Arc;
use std::time::Duration;

use databrain_auth::SecretStore;
use databrain_workspace::Workspace;
use futures::StreamExt;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::agent::provider_for;
use crate::types::*;
use crate::{access, knowledge};

/// What the editor sends: the text around the cursor.
#[derive(Debug, Clone, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct GhostRequest {
    pub connection_id: String,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Text before the cursor (the editor sends the last few thousand characters).
    pub prefix: String,
    /// Text after the cursor.
    #[serde(default)]
    pub suffix: String,
}

const MAX_PREFIX: usize = 4_000;
const MAX_SUFFIX: usize = 1_500;
const TIMEOUT: Duration = Duration::from_secs(15);

fn tail(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut i = s.len() - n;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

fn head(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut i = n;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// The model's answer without code fences or a leading "<CURSOR>" echo.
pub fn clean(raw: &str) -> String {
    let mut t = raw.trim_end().to_string();
    if let Some(rest) = t.trim_start().strip_prefix("```") {
        // ```sql\n…\n```
        let body = rest.split_once('\n').map(|(_, b)| b).unwrap_or("");
        t = body.trim_end().trim_end_matches("```").trim_end().to_string();
    }
    t.replace("<CURSOR>", "")
}

/// Complete the SQL at the cursor. Returns the text to insert (may be empty).
pub async fn complete(ws: &Workspace, secrets: Arc<dyn SecretStore>, req: &GhostRequest, cancel: &CancellationToken) -> Result<String> {
    let profile = ws.get_connection(&req.connection_id)?;
    if !profile.ai_policy.ai_enabled {
        return Err(AiError::Policy("AI is disabled for this connection (Connection → AI settings)".into()));
    }
    let rec = match &req.provider_id {
        Some(id) => ws.get_ai_provider(id)?,
        None => ws.list_ai_providers()?.into_iter().next().ok_or_else(|| AiError::Config("add an AI provider first (AI panel → Providers)".into()))?,
    };
    let allowed = &profile.ai_policy.allowed_providers;
    if !allowed.is_empty() && !allowed.contains(&rec.id) {
        return Err(AiError::Policy(format!("provider \"{}\" is not allowed for this connection", rec.name)));
    }
    let (provider, cfg, _) = provider_for(&rec, secrets)?;
    if provider.as_kiro().is_some() {
        return Err(AiError::Config("inline completion needs a model provider; Kiro runs as an agent (choose another provider in the AI panel)".into()));
    }
    // The fast model when the provider has one.
    let model = cfg
        .fast_model
        .clone()
        .filter(|m| !m.trim().is_empty())
        .or(req.model.clone().filter(|m| !m.trim().is_empty()))
        .or(cfg.default_model.clone())
        .ok_or_else(|| AiError::Config(format!("choose a model for \"{}\"", rec.name)))?;

    let prefix = tail(&req.prefix, MAX_PREFIX);
    let suffix = head(&req.suffix, MAX_SUFFIX);
    // A few relevant tables (names and columns only as the policy allows);
    // none when a table rule would hide one of them.
    let question = format!("{} {}", tail(prefix, 1_500), head(suffix, 300));
    let schema = match knowledge::retrieve(ws, &profile.id, &question, 5, 4_000) {
        Ok(r) if !r.tables.iter().any(|t| access::check_read_table(&profile, t).is_err()) => r.text,
        _ => String::new(),
    };
    let dialect = profile.config.kind.dialect_name();
    let system = format!(
        "You are an autocomplete engine inside a SQL editor. Continue the user's {dialect} at <CURSOR>. \
         Reply with ONLY the text to insert at the cursor: no explanation, no code fences, nothing the user already typed. \
         Finish the current clause or statement (at most 6 lines); stop at the end of the statement. \
         Use only tables and columns from the schema or the SQL itself. If no continuation is clear, reply with nothing."
    );
    let user =
        format!("{}SQL (cursor at <CURSOR>):\n{prefix}<CURSOR>{suffix}", if schema.is_empty() { String::new() } else { format!("Schema:\n{schema}\n\n") });
    let chat = ChatRequest { model, system, messages: vec![Message::user(user)], tools: vec![], temperature: Some(0.0), max_tokens: Some(160) };

    let run = async {
        let mut stream = provider.chat_stream(chat).await?;
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            match ev? {
                ChatEvent::TextDelta(t) => {
                    text.push_str(&t);
                    if text.len() > 2_000 {
                        break;
                    }
                }
                ChatEvent::Done { .. } => break,
                _ => {}
            }
        }
        Ok::<_, AiError>(text)
    };
    let text = tokio::select! {
        r = tokio::time::timeout(TIMEOUT, run) => r.map_err(|_| AiError::Network("the model took too long".into()))??,
        _ = cancel.cancelled() => return Err(AiError::Cancelled),
    };
    Ok(clean(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_answers() {
        assert_eq!(clean("```sql\nFROM orders\n```"), "FROM orders");
        assert_eq!(clean(" FROM orders\n"), " FROM orders");
        assert_eq!(clean("<CURSOR>id = 1"), "id = 1");
        assert_eq!(tail("ééé", 3), "é");
        assert_eq!(head("ééé", 3), "é");
    }
}
