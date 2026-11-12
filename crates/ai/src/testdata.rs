//! "Generate with AI" for test data: a model writes realistic rows for the
//! columns random values can't make sense of (names, emails, addresses,
//! descriptions), or for every column it may write. Only the table's
//! structure is sent (column names, types, limits and the CHECK constraints
//! on those columns), never a row of it, and only where the connection
//! shares metadata with the AI. The rows come back as JSON; the query engine checks each value
//! against its column and makes up the ones that don't fit, and keys and
//! foreign keys never come from the model.

use std::sync::Arc;
use std::time::Duration;

use databrain_auth::SecretStore;
use databrain_query_engine::testdata::{AiAsk, AiSample};
use databrain_workspace::Workspace;
use futures::StreamExt;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::access;
use crate::agent::provider_for;
use crate::meter::Meter;
use crate::types::*;

/// How "Generate test data" uses the AI (the dialog's settings).
#[derive(Debug, Clone, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct TestDataAi {
    /// The AI panel's provider; `None`: the first one.
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Every column the AI may write, not only text.
    #[serde(default)]
    pub all_columns: bool,
    /// Rows the AI writes; more rows reuse them (cost stays flat).
    pub rows: u32,
}

/// Rows per model call: small enough to come back whole.
const BATCH_ROWS: usize = 25;
/// Calls at a time.
const PARALLEL: usize = 3;
const TIMEOUT: Duration = Duration::from_secs(120);
/// Output tokens per call: within what common models allow.
const MAX_TOKENS: usize = 8_000;

/// The model to ask, after the connection's AI policy allowed it.
struct Target {
    provider: Arc<dyn crate::providers::LlmProvider>,
    provider_id: String,
    model: String,
    /// The table's CHECK constraints that may be sent (none on PII columns).
    checks: Vec<String>,
}

fn target(ws: &Workspace, secrets: Arc<dyn SecretStore>, connection_id: &str, ai: &TestDataAi, ask: &AiAsk) -> Result<Target> {
    let profile = ws.get_connection(connection_id)?;
    if !profile.ai_policy.ai_enabled {
        return Err(AiError::Policy("AI is disabled for this connection (Connection → AI settings)".into()));
    }
    if !profile.ai_policy.share_metadata {
        return Err(AiError::Policy(
            "this connection doesn't share table structure with the AI (Connection → AI → Share schema metadata), and the AI needs it to write rows".into(),
        ));
    }
    // A table the AI may not read: its structure isn't sent either.
    let table = format!("{}.{}", ask.schema, ask.name);
    access::check_read_table(&profile, &table).map_err(AiError::Policy)?;
    // CHECK constraints on a PII column could carry real values: left out.
    let checks: Vec<String> = ask
        .checks
        .iter()
        .filter(|c| !c.columns.iter().any(|col| crate::policy::is_pii(&profile.ai_policy, Some(&table), col)))
        .map(|c| c.sql.clone())
        .collect();
    let rec = match &ai.provider_id {
        Some(id) => ws.get_ai_provider(id)?,
        None => ws.list_ai_providers()?.into_iter().next().ok_or_else(|| AiError::Config("add an AI provider first (AI panel → Providers)".into()))?,
    };
    let allowed = &profile.ai_policy.allowed_providers;
    if !allowed.is_empty() && !allowed.contains(&rec.id) {
        return Err(AiError::Policy(format!("provider \"{}\" is not allowed for this connection", rec.name)));
    }
    let (provider, cfg, _) = provider_for(&rec, secrets)?;
    if provider.as_kiro().is_some() {
        return Err(AiError::Config("generating test data needs a model provider, and Kiro runs as an agent: choose another provider in the AI panel".into()));
    }
    let some = |m: Option<&str>| m.map(str::trim).filter(|m| !m.is_empty()).map(str::to_string);
    let model = some(ai.model.as_deref())
        .or_else(|| some(cfg.default_model.as_deref()))
        .or_else(|| some(cfg.fast_model.as_deref()))
        .ok_or_else(|| AiError::Config(format!("choose a model for \"{}\"", rec.name)))?;
    Ok(Target { provider, provider_id: rec.id, model, checks })
}

/// One call: the answer's text. Its tokens go to `meter` as they come (read
/// to the end of the stream: providers report usage after Done), so a call
/// stopped half way is counted too.
async fn ask_model(provider: &Arc<dyn crate::providers::LlmProvider>, chat: ChatRequest, meter: &Meter) -> Result<String> {
    let run = async {
        let mut stream = provider.chat_stream(chat.clone()).await?;
        meter.sent(&chat);
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            match ev? {
                ChatEvent::TextDelta(t) => {
                    meter.text(&t);
                    text.push_str(&t);
                }
                ChatEvent::Usage { input, output } => meter.reported(input, output),
                _ => {}
            }
        }
        meter.finished();
        Ok::<_, AiError>(text)
    };
    tokio::time::timeout(TIMEOUT, run).await.map_err(|_| AiError::Network("the model took too long".into()))?
}

fn record(ws: &Workspace, provider_id: &str, used: (u32, u32)) {
    if let Err(e) = ws.record_ai_usage(provider_id, "test_data", used.0, used.1) {
        tracing::warn!("couldn't record AI usage: {e}");
    }
}

/// Patterns the AI suggests for the columns of `ask` (see
/// [`databrain_query_engine::testdata::PATTERN_HELP`]), by column name.
/// Unchecked: the engine keeps the ones that parse.
pub async fn suggest_patterns(
    ws: &Workspace,
    secrets: Arc<dyn SecretStore>,
    connection_id: &str,
    ai: &TestDataAi,
    ask: &AiAsk,
    cancel: &CancellationToken,
) -> Result<std::collections::HashMap<String, String>> {
    let t = target(ws, secrets, connection_id, ai, ask)?;
    let (system, user) = suggest_prompt(ask, &t.checks);
    let chat =
        ChatRequest { model: t.model.clone(), system, messages: vec![Message::user(user)], tools: vec![], temperature: Some(0.2), max_tokens: Some(2_000) };
    let meter = Meter::default();
    let r = tokio::select! {
        r = ask_model(&t.provider, chat, &meter) => r,
        _ = cancel.cancelled() => Err(AiError::Cancelled),
    };
    record(ws, &t.provider_id, meter.tokens());
    let text = r?;
    parse_patterns(&text, ask).ok_or_else(|| AiError::Provider("the model's answer wasn't the JSON object of patterns asked for".into()))
}

fn suggest_prompt(ask: &AiAsk, checks: &[String]) -> (String, String) {
    let system = format!(
        "You set rules for generating realistic test data for a database table. Reply with ONLY a JSON object mapping column names to patterns: \
         no explanation, no code fences. Pattern syntax: {}",
        databrain_query_engine::testdata::PATTERN_HELP
    );
    let mut u = format!("Table {} ({}).\n\nColumns:\n", ask.table, ask.dialect);
    for c in &ask.columns {
        let mut parts = vec![c.data_type.clone()];
        if let Some(m) = c.max_chars {
            parts.push(format!("at most {m} characters"));
        }
        if !c.choices.is_empty() {
            parts.push(format!("one of: {}", c.choices.join(", ")));
        }
        parts.push(match &c.pattern {
            Some(p) => format!("current pattern {p}"),
            None => format!("now gets {}", c.hint),
        });
        u.push_str(&format!("- {}: {}\n", c.name, parts.join("; ")));
    }
    if !checks.is_empty() {
        u.push_str("\nThe table's CHECK constraints (patterns must keep to them):\n");
        for c in checks {
            u.push_str(&format!("- {c}\n"));
        }
    }
    u.push_str(
        "\nGive a pattern for each column whose values would otherwise look wrong: ranges for ages, years, prices, quantities, ratings and \
         dates of birth; templates for emails, phone numbers, codes and references, and dates kept as text. Keep a current pattern \
         when it is right. Leave out columns that need none.",
    );
    (system, u)
}

/// The `{"column": "pattern"}` object in the model's answer, for columns of `ask`.
fn parse_patterns(text: &str, ask: &AiAsk) -> Option<std::collections::HashMap<String, String>> {
    let end = text.rfind('}')?;
    let obj: serde_json::Map<String, serde_json::Value> =
        text.match_indices('{').map(|(i, _)| i).filter(|&i| i < end).find_map(|i| serde_json::from_str(text.get(i..=end)?).ok())?;
    Some(
        obj.into_iter()
            .filter_map(|(k, v)| {
                let name = ask.columns.iter().find(|c| c.name.eq_ignore_ascii_case(&k))?.name.clone();
                let p = v.as_str()?.trim().to_string();
                (!p.is_empty()).then_some((name, p))
            })
            .collect(),
    )
}

/// Ask the model for `ask.rows` rows, in batches.
pub async fn sample(
    ws: &Workspace,
    secrets: Arc<dyn SecretStore>,
    connection_id: &str,
    ai: &TestDataAi,
    ask: &AiAsk,
    cancel: &CancellationToken,
) -> Result<AiSample> {
    let Target { provider, provider_id, model, checks } = target(ws, secrets, connection_id, ai, ask)?;
    let batches = ask.rows.div_ceil(BATCH_ROWS).max(1);
    // Each call's tokens: those that came back, those cut off by Stop, and
    // those that failed (counted in the provider's usage either way).
    let mut meters: Vec<Meter> = Vec::new();
    let calls: Vec<_> = (0..batches)
        .map(|b| {
            let n = BATCH_ROWS.min(ask.rows - b * BATCH_ROWS);
            let (system, user) = prompt(ask, &checks, b, batches, n);
            let chat = ChatRequest {
                model: model.clone(),
                system,
                messages: vec![Message::user(user)],
                tools: vec![],
                temperature: Some(0.9),
                max_tokens: Some(max_tokens(ask, n)),
            };
            let provider = provider.clone();
            let meter = Meter::default();
            meters.push(meter.clone());
            async move { ask_model(&provider, chat, &meter).await }
        })
        .collect();
    let columns: Vec<String> = ask.columns.iter().map(|c| c.name.clone()).collect();
    let mut stream = futures::stream::iter(calls).buffer_unordered(PARALLEL);
    let mut rows = Vec::new();
    let mut last_error = None;
    let record = || record(ws, &provider_id, crate::meter::total(&meters));
    loop {
        let next = tokio::select! {
            n = stream.next() => n,
            _ = cancel.cancelled() => {
                drop(stream);
                record();
                return Err(AiError::Cancelled);
            }
        };
        let Some(r) = next else { break };
        match r {
            Ok(text) => match parse_rows(&text, &columns) {
                Some(mut r) => rows.append(&mut r),
                None => last_error = Some(AiError::Provider("the model's answer wasn't the JSON rows asked for".into())),
            },
            Err(e) => last_error = Some(e),
        }
    }
    drop(stream);
    record();
    if rows.is_empty() {
        return Err(last_error.unwrap_or_else(|| AiError::Provider("the model wrote no rows".into())));
    }
    rows.truncate(ask.rows);
    Ok(AiSample { columns, rows })
}

/// Enough tokens for `n` rows of the columns, with room to spare.
fn max_tokens(ask: &AiAsk, n: usize) -> u32 {
    let per_row: usize = ask.columns.iter().map(|c| 8 + c.max_chars.unwrap_or(80).min(200) / 3).sum();
    (n * per_row + 500).clamp(1_000, MAX_TOKENS) as u32
}

/// The instructions and the request for batch `batch` (0-based) of `batches`.
fn prompt(ask: &AiAsk, checks: &[String], batch: usize, batches: usize, n: usize) -> (String, String) {
    let system = "You write realistic, varied test data for a database table. \
        Reply with ONLY a JSON array of row objects: no explanation, no code fences. \
        Use exactly the keys asked for. Values look like real data for the column's name and type \
        (people's names, real-looking emails and addresses, plausible descriptions), not placeholders like \"test\" or \"string1\"."
        .to_string();
    let mut u = format!("Table {} ({}).\n", ask.table, ask.dialect);
    u.push_str("\nColumns to write (the JSON keys):\n");
    for c in &ask.columns {
        let mut rules = vec![c.data_type.clone()];
        if c.unique {
            rules.push("unique: no value twice".into());
        }
        if let Some(m) = c.max_chars {
            rules.push(format!("at most {m} characters"));
        }
        if !c.choices.is_empty() {
            rules.push(format!("one of: {}", c.choices.join(", ")));
        }
        match &c.pattern {
            Some(p) => rules.push(format!("follows the pattern {p}")),
            None => rules.push(format!("for example {}", c.hint)),
        }
        u.push_str(&format!("- {}: {}\n", c.name, rules.join("; ")));
    }
    if ask.columns.iter().any(|c| c.pattern.is_some()) {
        u.push_str(&format!("Pattern syntax: {}\n", databrain_query_engine::testdata::PATTERN_HELP));
    }
    if !checks.is_empty() {
        u.push_str("\nEvery row must satisfy:\n");
        for c in checks {
            u.push_str(&format!("- {c}\n"));
        }
    }
    if !ask.others.is_empty() {
        u.push_str("\nOther columns, filled by DataBrain (don't write them; use them only as context):\n");
        for o in &ask.others {
            u.push_str(&format!("- {o}\n"));
        }
    }
    u.push_str(&format!(
        "\nWrite {n} rows{}. Values in one row belong together (an email matches the name). \
         Dates as YYYY-MM-DD, date-times as YYYY-MM-DD HH:MM:SS, times as HH:MM:SS, numbers as JSON numbers, \
         JSON columns as a JSON string. Never write null: DataBrain adds NULLs itself.",
        if batches > 1 { format!(" (batch {} of {batches}: make them different from the other batches' rows)", batch + 1) } else { String::new() }
    ));
    (system, u)
}

/// The rows in the model's answer, as text per column (`None`: null or
/// missing). `None` when there is no JSON array of objects in it.
fn parse_rows(text: &str, columns: &[String]) -> Option<Vec<Vec<Option<String>>>> {
    // From the first `[` that starts the array (not one in a preamble like "[batch 2]").
    let end = text.rfind(']')?;
    let items: Vec<serde_json::Value> =
        text.match_indices('[').map(|(i, _)| i).filter(|&i| i < end).find_map(|i| serde_json::from_str(text.get(i..=end)?).ok())?;
    let rows: Vec<Vec<Option<String>>> = items
        .into_iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            Some(
                columns
                    .iter()
                    .map(|c| {
                        let v = obj.get(c).or_else(|| obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(c)).map(|(_, v)| v))?;
                        match v {
                            serde_json::Value::Null => None,
                            serde_json::Value::String(s) => Some(s.clone()),
                            other => Some(other.to_string()),
                        }
                    })
                    .collect(),
            )
        })
        .collect();
    (!rows.is_empty()).then_some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_query_engine::testdata::AiColumn;

    /// Writes some text, then (unless `ends`) never finishes; reports usage
    /// only at the end, as providers do.
    struct Slow {
        ends: bool,
    }

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for Slow {
        fn kind(&self) -> crate::providers::ProviderKind {
            crate::providers::ProviderKind::Openai
        }
        async fn list_models(&self) -> Result<Vec<ModelInfo>> {
            Ok(vec![])
        }
        async fn chat_stream(&self, _: ChatRequest) -> Result<futures::stream::BoxStream<'static, Result<ChatEvent>>> {
            let head = futures::stream::iter(vec![Ok(ChatEvent::TextDelta("x".repeat(80)))]);
            if self.ends {
                Ok(head.chain(futures::stream::iter(vec![Ok(ChatEvent::Usage { input: 7, output: 3 })])).boxed())
            } else {
                Ok(head.chain(futures::stream::pending()).boxed())
            }
        }
    }

    #[tokio::test]
    async fn stopped_calls_are_counted_by_estimate() {
        let chat = || ChatRequest { model: "m".into(), system: "s".repeat(400), messages: vec![], tools: vec![], temperature: None, max_tokens: None };
        let stopped: Arc<dyn crate::providers::LlmProvider> = Arc::new(Slow { ends: false });
        let meter = Meter::default();
        let r = tokio::time::timeout(Duration::from_millis(50), ask_model(&stopped, chat(), &meter)).await;
        assert!(r.is_err(), "still running when stopped");
        assert_eq!(meter.tokens(), (100, 20), "400 prompt and 80 answer characters");
        // A finished call counts what the provider reported.
        let done: Arc<dyn crate::providers::LlmProvider> = Arc::new(Slow { ends: true });
        let meter = Meter::default();
        assert_eq!(ask_model(&done, chat(), &meter).await.unwrap().len(), 80);
        assert_eq!(meter.tokens(), (7, 3));
    }

    fn ask() -> AiAsk {
        AiAsk {
            schema: "public".into(),
            name: "customers".into(),
            table: "\"public\".\"customers\"".into(),
            dialect: "PostgreSQL".into(),
            checks: vec![],
            columns: vec![
                AiColumn {
                    name: "name".into(),
                    data_type: "varchar(40)".into(),
                    nullable: false,
                    unique: false,
                    max_chars: Some(40),
                    choices: vec![],
                    hint: "names (up to 40 characters)".into(),
                    pattern: None,
                },
                AiColumn {
                    name: "email".into(),
                    data_type: "text".into(),
                    nullable: true,
                    unique: true,
                    max_chars: None,
                    choices: vec![],
                    hint: "emails".into(),
                    pattern: Some("%@gmail.com".into()),
                },
            ],
            others: vec!["id integer (numbers counting up from the largest)".into(), "country_id integer (existing keys of a parent table)".into()],
            rows: 60,
        }
    }

    #[test]
    fn the_prompt_has_the_structure_and_rules_but_no_rows() {
        let checks = vec!["CHECK (tier IN ('gold','silver'))".to_string()];
        let (system, user) = prompt(&ask(), &checks, 1, 3, 25);
        assert!(system.contains("ONLY a JSON array"));
        assert!(user.contains("Every row must satisfy:\n- CHECK (tier IN"), "{user}");
        assert!(user.contains("- name: varchar(40); at most 40 characters"), "{user}");
        assert!(user.contains("- email: text; unique: no value twice; follows the pattern %@gmail.com"), "{user}");
        assert!(user.contains("Pattern syntax:"), "{user}");
        assert!(user.contains("Never write null"), "{user}");
        assert!(user.contains("country_id integer (existing keys of a parent table)"), "{user}");
        assert!(user.contains("Write 25 rows (batch 2 of 3"), "{user}");
        let (_, one) = prompt(&AiAsk { rows: 10, ..ask() }, &[], 0, 1, 10);
        assert!(one.contains("Write 10 rows."), "{one}");
        assert!(!one.contains("Every row must satisfy"), "{one}");
    }

    #[test]
    fn pattern_suggestions_are_asked_for_and_read() {
        let (system, user) = suggest_prompt(&ask(), &["CHECK (tier IN ('gold','silver'))".into()]);
        assert!(system.contains("ONLY a JSON object") && system.contains("18..90"), "{system}");
        assert!(user.contains("- name: varchar(40); at most 40 characters; now gets names"), "{user}");
        assert!(user.contains("- email: text; current pattern %@gmail.com"), "{user}");
        assert!(user.contains("CHECK (tier IN"), "{user}");
        let got = parse_patterns("Sure {x}:\n{\"NAME\": \"% %\", \"email\": \"%@corp.vn\", \"other\": \"1..2\", \"blank\": \"\"}", &ask()).unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got["name"], "% %", "the column's own spelling");
        assert_eq!(got["email"], "%@corp.vn");
        assert!(parse_patterns("no", &ask()).is_none());
    }

    #[test]
    fn answers_are_read_leniently() {
        let cols = vec!["name".to_string(), "email".to_string(), "age".to_string()];
        let text = "Here you go:\n```json\n[{\"name\": \"Ann Lee\", \"EMAIL\": \"ann@x.com\", \"age\": 31}, {\"name\": \"Bo\", \"email\": null}, 7]\n```";
        let rows = parse_rows(text, &cols).unwrap();
        assert_eq!(rows, vec![vec![Some("Ann Lee".into()), Some("ann@x.com".into()), Some("31".into())], vec![Some("Bo".into()), None, None]]);
        assert_eq!(parse_rows("I can't do that.", &cols), None);
        assert_eq!(parse_rows("[1, 2]", &cols), None);
        assert_eq!(parse_rows("[{\"name\": ", &cols), None);
        let rows = parse_rows("Rows [batch 2]:\n[{\"name\": \"Cy\"}]", &cols).unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("Cy"), "a bracket in the preamble");
    }

    #[test]
    fn token_budget_grows_with_rows_and_is_capped() {
        let a = ask();
        assert!(max_tokens(&a, 25) > max_tokens(&a, 5));
        assert!(max_tokens(&a, 25) <= 8_000);
        assert!(max_tokens(&a, 1) >= 1_000);
    }
}
