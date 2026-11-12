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
    /// `model` was chosen for inline completion (Settings → Editor): used
    /// over the provider's fast model. Otherwise it is the AI panel's model,
    /// and the fast model comes first.
    #[serde(default)]
    pub exact_model: bool,
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

/// A model chosen for inline completion (`exact`) first; otherwise the
/// provider's fast model before the AI panel's; then the default model.
fn pick_model(requested: Option<&str>, exact: bool, fast: Option<&str>, default: Option<&str>) -> Option<String> {
    let some = |m: Option<&str>| m.map(str::trim).filter(|m| !m.is_empty()).map(str::to_string);
    let (requested, fast) = (some(requested), some(fast));
    let first = if exact { requested.or(fast) } else { fast.or(requested) };
    first.or(some(default))
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
        return Err(AiError::Config(
            "inline completion needs a model provider, and Kiro runs as an agent: choose a provider for inline completion in Settings → Editor".into(),
        ));
    }
    let model = pick_model(req.model.as_deref(), req.exact_model, cfg.fast_model.as_deref(), cfg.default_model.as_deref())
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

    // Tokens counted in the provider's usage: as reported, or estimated
    // when typing cancels the call before the report comes.
    let meter = crate::meter::Meter::default();
    let run = async {
        let mut stream = provider.chat_stream(chat.clone()).await?;
        meter.sent(&chat);
        let mut text = String::new();
        // Providers report usage after Done: read to the end.
        while let Some(ev) = stream.next().await {
            // An error the provider sends ends the call: what it reported
            // counts, no estimate. A broken connection keeps the estimate.
            let ev = ev.inspect_err(|e| {
                if !matches!(e, AiError::Network(_)) {
                    meter.finished()
                }
            })?;
            match ev {
                ChatEvent::TextDelta(t) => {
                    meter.text(&t);
                    if text.len() <= 2_000 {
                        text.push_str(&t);
                    }
                }
                ChatEvent::Usage { input, output } => meter.reported(input, output),
                _ => {}
            }
        }
        meter.finished();
        Ok::<_, AiError>(text)
    };
    let r = tokio::select! {
        r = tokio::time::timeout(TIMEOUT, run) => r.map_err(|_| AiError::Network("the model took too long".into())).and_then(|r| r),
        _ = cancel.cancelled() => Err(AiError::Cancelled),
    };
    let (i, o) = meter.tokens();
    if let Err(e) = ws.record_ai_usage(&rec.id, "inline", i, o) {
        tracing::warn!("couldn't record AI usage: {e}");
    }
    Ok(clean(&r?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_chosen_for_inline_completion_wins_over_the_fast_model() {
        assert_eq!(pick_model(Some("gpt-big"), false, Some("gpt-mini"), Some("gpt-x")).as_deref(), Some("gpt-mini"));
        assert_eq!(pick_model(Some("gpt-big"), true, Some("gpt-mini"), Some("gpt-x")).as_deref(), Some("gpt-big"));
        assert_eq!(pick_model(None, true, Some("gpt-mini"), Some("gpt-x")).as_deref(), Some("gpt-mini"));
        assert_eq!(pick_model(Some(" "), true, None, Some("gpt-x")).as_deref(), Some("gpt-x"));
        assert_eq!(pick_model(None, false, None, None), None);
    }

    #[test]
    fn cleans_answers() {
        assert_eq!(clean("```sql\nFROM orders\n```"), "FROM orders");
        assert_eq!(clean(" FROM orders\n"), " FROM orders");
        assert_eq!(clean("<CURSOR>id = 1"), "id = 1");
        assert_eq!(tail("ééé", 3), "é");
        assert_eq!(head("ééé", 3), "é");
    }
}
