//! AI mode, knowledge and sign-in API used by Tauri commands.
//!
//! The UI is reached through [`UiBridge`] (Tauri events in the app, a
//! recorder in tests). Approvals and editor proposals are request/response:
//! the backend emits a request with an id and waits for the UI to answer via
//! `ai_respond`.

use databrain_auth::terminal::{Launch, TerminalJob, TerminalSetting};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use databrain_ai::learn::{self, LearnReport};
use databrain_ai::{Agent, AgentEvent, AgentRequest, AgentSink, ModelInfo, ProviderAuth, ProviderConfig, ProviderKind, ToolHost};
use databrain_auth::{AuthError, AuthStatus, CancellationToken, DeviceCodePrompt, Interaction, SecretRef, SecretString};
use databrain_query_engine::{EngineError, Result};
use databrain_workspace::{
    AiMessageRecord, AiProviderRecord, AiSessionRecord, AuditEntry, JoinStatus, KnNote, KnObject, KnState, LearnState, LearnedJoin, TableUsage,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::api::AppState;

/// Events pushed to the UI.
pub trait UiBridge: Send + Sync {
    /// `channel` is the Tauri event name.
    fn emit(&self, channel: &str, payload: Value);
    fn open_url(&self, url: &str) -> std::result::Result<(), String>;
}

pub const AI_EVENT: &str = "ai-event";
pub const AUTH_EVENT: &str = "auth-event";
pub const KNOWLEDGE_EVENT: &str = "knowledge-event";

#[derive(Default, Clone)]
pub struct AiState {
    /// Pending UI requests (approval / editor proposal / editor state).
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    /// Running agent turns, cancellable by run id.
    runs: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// Cancels the in-progress interactive sign-in.
    pub sign_in_cancel: Arc<Mutex<CancellationToken>>,
    /// Knowledge index runs, cancellable by connection id.
    indexing: Arc<Mutex<HashMap<String, (u64, CancellationToken)>>>,
}

impl AiState {
    async fn ask(&self, ui: &dyn UiBridge, kind: &str, payload: Value, timeout: Duration) -> Option<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id.clone(), tx);
        // Removed however this ends: answered, timed out, or dropped (Stop).
        struct Forget<'a>(&'a Mutex<std::collections::HashMap<String, oneshot::Sender<Value>>>, String);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                self.0.lock().remove(&self.1);
            }
        }
        let _forget = Forget(&self.pending, id.clone());
        let mut p = payload;
        p["type"] = kind.into();
        p["request_id"] = id.clone().into();
        ui.emit(AI_EVENT, p);
        tokio::time::timeout(timeout, rx).await.ok().and_then(|r| r.ok())
    }
}

// ------------------------------------------------------------------ sign-in interaction

pub struct AppInteraction {
    ui: Arc<dyn UiBridge>,
    cancel: Arc<Mutex<CancellationToken>>,
}

impl AppInteraction {
    pub fn new(ui: Arc<dyn UiBridge>, cancel: Arc<Mutex<CancellationToken>>) -> Self {
        Self { ui, cancel }
    }
}

#[async_trait]
impl Interaction for AppInteraction {
    async fn open_url(&self, url: &str) -> std::result::Result<(), AuthError> {
        *self.cancel.lock() = CancellationToken::new();
        self.ui.emit(AUTH_EVENT, json!({"type": "browser_opened", "url": url}));
        self.ui.open_url(url).map_err(AuthError::Flow)
    }
    async fn device_code(&self, prompt: &DeviceCodePrompt) {
        *self.cancel.lock() = CancellationToken::new();
        let _ = self.ui.open_url(prompt.verification_uri_complete.as_deref().unwrap_or(&prompt.verification_uri));
        self.ui.emit(AUTH_EVENT, json!({"type": "device_code", "prompt": prompt}));
    }
    async fn finished(&self) {
        self.ui.emit(AUTH_EVENT, json!({"type": "finished"}));
    }
    fn cancel_token(&self) -> CancellationToken {
        self.cancel.lock().clone()
    }
}

pub async fn sign_in(state: &AppState, connection_id: &str) -> Result<AuthStatus> {
    state.engine.disconnect(connection_id);
    state.engine.sign_in(connection_id).await
}

pub async fn sign_out(state: &AppState, connection_id: &str) -> Result<()> {
    state.engine.sign_out(connection_id).await
}

pub fn auth_status(state: &AppState, connection_id: &str) -> Result<AuthStatus> {
    state.engine.auth_status(connection_id)
}

pub fn cancel_sign_in(state: &AppState) {
    state.ai.sign_in_cancel.lock().cancel();
}

// ------------------------------------------------------------------ providers

fn ai_err(e: databrain_ai::AiError) -> EngineError {
    let kind = serde_json::to_value(&e).ok().and_then(|v| v["kind"].as_str().map(str::to_string)).unwrap_or_else(|| "internal".into());
    EngineError::new(&kind, e.to_string())
}

#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ProviderView {
    #[serde(flatten)]
    pub record: AiProviderRecord,
    pub has_key: bool,
}

pub fn list_providers(state: &AppState) -> Result<Vec<ProviderView>> {
    Ok(state
        .workspace
        .list_ai_providers()?
        .into_iter()
        .map(|r| ProviderView { has_key: state.secrets.get(&SecretRef::for_ai_provider(&r.id)).ok().flatten().is_some(), record: r })
        .collect())
}

#[derive(Debug, Deserialize)]
pub struct SaveProviderArgs {
    pub record: AiProviderRecord,
    /// New API key; empty string clears it; absent keeps it.
    #[serde(default)]
    pub api_key: Option<String>,
}

pub fn save_provider(state: &AppState, args: SaveProviderArgs) -> Result<AiProviderRecord> {
    let kind = ProviderKind::parse(&args.record.kind).ok_or_else(|| EngineError::new("invalid", "unknown provider type"))?;
    let cfg: ProviderConfig = serde_json::from_value(args.record.config.clone()).map_err(|e| EngineError::new("invalid", e.to_string()))?;
    if kind == ProviderKind::Kiro && cfg.cli_env.keys().any(|k| k.eq_ignore_ascii_case("KIRO_API_KEY")) {
        return Err(EngineError::new("invalid", "store the Kiro API key in the API key field (keychain), not in the environment"));
    }
    if kind == ProviderKind::AzureOpenai && cfg.base_url.as_deref().is_none_or(str::is_empty) {
        return Err(EngineError::new("invalid", "Azure OpenAI needs the resource endpoint URL"));
    }
    // Validate the key before saving anything.
    let key = match args.api_key.as_deref().map(str::trim) {
        Some(k) if !k.is_empty() && kind == ProviderKind::Kiro => Some(databrain_ai::kiro::normalize_api_key(k).map_err(ai_err)?),
        other => other.map(str::to_string),
    };
    let saved = state.workspace.save_ai_provider(args.record)?;
    let r = SecretRef::for_ai_provider(&saved.id);
    match key.as_deref() {
        Some("") => state.secrets.delete(&r)?,
        Some(k) => state.secrets.set(&r, &SecretString::from(k.to_string()))?,
        None => {}
    }
    Ok(saved)
}

pub fn delete_provider(state: &AppState, id: &str) -> Result<()> {
    state.workspace.delete_ai_provider(id)?;
    state.secrets.delete(&SecretRef::for_ai_provider(id))?;
    Ok(())
}

pub async fn list_models(state: &AppState, id: &str) -> Result<Vec<ModelInfo>> {
    let rec = state.workspace.get_ai_provider(id)?;
    let (p, _, _) = databrain_ai::provider_for(&rec, state.secrets.clone()).map_err(ai_err)?;
    p.list_models().await.map_err(ai_err)
}

/// Budget and token use of a provider (Settings, AI panel).
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ProviderUsage {
    /// Credits/balance, when the provider reports them.
    pub budget: Option<databrain_ai::Budget>,
    /// Why the budget couldn't be read (sign-in, network…).
    pub budget_error: Option<String>,
    /// Whether this provider has a budget API at all.
    pub budget_supported: bool,
    /// Tokens (input, output) of DataBrain's conversations with it.
    #[ts(as = "(f64, f64)")]
    pub tokens_today: (i64, i64),
    #[ts(as = "(f64, f64)")]
    pub tokens_month: (i64, i64),
}

/// `since_day` / `since_month`: local midnight of today and of the 1st (ms).
pub async fn provider_usage(state: &AppState, id: &str, since_day: i64, since_month: i64) -> Result<ProviderUsage> {
    let rec = state.workspace.get_ai_provider(id)?;
    let kind = ProviderKind::parse(&rec.kind);
    let cfg: ProviderConfig = serde_json::from_value(rec.config.clone()).unwrap_or_default();
    let deepseek = cfg.base_url.as_deref().is_some_and(|u| u.contains("api.deepseek.com"));
    let budget_supported = matches!(kind, Some(ProviderKind::Kiro | ProviderKind::Openrouter)) || deepseek;
    let (mut budget, mut budget_error) = (None, None);
    if budget_supported {
        let (p, _, _) = databrain_ai::provider_for(&rec, state.secrets.clone()).map_err(ai_err)?;
        match tokio::time::timeout(Duration::from_secs(30), p.budget()).await {
            Ok(Ok(b)) => budget = b,
            Ok(Err(e)) => budget_error = Some(e.to_string()),
            Err(_) => budget_error = Some("the provider didn't answer in time".into()),
        }
    }
    Ok(ProviderUsage {
        budget,
        budget_error,
        budget_supported,
        tokens_today: state.workspace.ai_token_totals(id, since_day)?,
        tokens_month: state.workspace.ai_token_totals(id, since_month)?,
    })
}

fn kiro_provider(state: &AppState, rec: &AiProviderRecord) -> Result<databrain_ai::kiro::KiroProvider> {
    let cfg: ProviderConfig = serde_json::from_value(rec.config.clone()).unwrap_or_default();
    let key = databrain_ai::providers::KeySource::Stored { store: state.secrets.clone(), reference: SecretRef::for_ai_provider(&rec.id) };
    databrain_ai::kiro::KiroProvider::new(&cfg, key).map_err(ai_err)
}

/// Kiro only: is kiro-cli installed, and signed in with the configured method?
pub async fn provider_status(state: &AppState, id: &str) -> Result<databrain_ai::kiro::KiroStatus> {
    let rec = state.workspace.get_ai_provider(id)?;
    if ProviderKind::parse(&rec.kind) != Some(ProviderKind::Kiro) {
        return Err(EngineError::new("invalid", "status is available for Kiro providers"));
    }
    Ok(kiro_provider(state, &rec)?.status().await)
}

/// Browser sign-in. OpenRouter: PKCE, stores the returned key. Kiro: runs
/// `kiro-cli login` in a terminal window and waits for the session.
pub async fn provider_sign_in(state: &AppState, id: &str) -> Result<()> {
    let mut rec = state.workspace.get_ai_provider(id)?;
    let kind = ProviderKind::parse(&rec.kind);
    let mut cfg: ProviderConfig = serde_json::from_value(rec.config.clone()).unwrap_or_default();
    match kind {
        Some(ProviderKind::Openrouter) => {
            let ui = AppInteraction::new(state.ui.clone(), state.ai.sign_in_cancel.clone());
            let key = databrain_ai::providers::openrouter_sign_in(&ui).await.map_err(ai_err)?;
            state.secrets.set(&SecretRef::for_ai_provider(id), &SecretString::from(key))?;
            cfg.auth = ProviderAuth::BrowserOpenrouter;
        }
        Some(ProviderKind::Kiro) => {
            let kiro = kiro_provider(state, &rec)?;
            let cancel = CancellationToken::new();
            *state.ai.sign_in_cancel.lock() = cancel.clone();
            let ui = state.ui.clone();
            let terminal = terminal_setting(state);
            kiro.sign_in_browser(cancel, &terminal, |l| ui.emit(AUTH_EVENT, launch_event(l, "kiro-cli login"))).await.map_err(ai_err)?;
            state.ui.emit(AUTH_EVENT, json!({"type": "finished"}));
            cfg.auth = ProviderAuth::KiroBrowser;
        }
        _ => return Err(EngineError::new("invalid", "browser sign-in is available for OpenRouter and Kiro; other providers use API keys")),
    }
    rec.config = serde_json::to_value(cfg).unwrap_or_default();
    state.workspace.save_ai_provider(rec)?;
    Ok(())
}

/// Settings → Terminal (`terminal`): which terminal runs CLI sign-ins.
pub fn terminal_setting(state: &AppState) -> TerminalSetting {
    state.workspace.get_setting("terminal").ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

/// Sign-in dialog event: a terminal runs `command`, or the user must run it.
fn launch_event(l: &Launch, short: &str) -> Value {
    match l {
        Launch::Opened { terminal } => json!({"type": "terminal_opened", "command": short, "terminal": terminal}),
        Launch::Manual { command, reason } => json!({"type": "run_command", "command": command, "reason": reason}),
    }
}

/// Settings → Terminal → Test: opens the terminal with a short message.
pub async fn terminal_test(setting: TerminalSetting) -> Launch {
    let job = TerminalJob {
        name: "terminal-test",
        title: "DataBrain terminal test",
        program: None,
        args: &[],
        unset_env: &[],
        done_message: "DataBrain can open this terminal. You can close this window.",
    };
    databrain_auth::terminal::launch(&setting, &job).await
}

/// Sign out: Kiro ends the kiro-cli browser session; others drop the stored key.
pub async fn provider_sign_out(state: &AppState, id: &str) -> Result<()> {
    let rec = state.workspace.get_ai_provider(id)?;
    if ProviderKind::parse(&rec.kind) == Some(ProviderKind::Kiro) {
        let cfg: ProviderConfig = serde_json::from_value(rec.config.clone()).unwrap_or_default();
        if cfg.auth == ProviderAuth::KiroBrowser {
            kiro_provider(state, &rec)?.sign_out().await.map_err(ai_err)?;
            return Ok(());
        }
    }
    state.secrets.delete(&SecretRef::for_ai_provider(id))?;
    Ok(())
}

// ------------------------------------------------------------------ agent

struct UiSink(Arc<dyn UiBridge>);

impl AgentSink for UiSink {
    fn emit(&self, e: AgentEvent) {
        self.0.emit(AI_EVENT, serde_json::to_value(e).unwrap_or_default());
    }
}

struct UiHost {
    ui: Arc<dyn UiBridge>,
    ai: AiState,
    run_id: String,
    result_id: Option<String>,
    tab_id: Option<String>,
    jobs: crate::jobs::JobCtx,
}

#[async_trait]
impl ToolHost for UiHost {
    async fn start_job(&self, job_id: &str, only: Option<Vec<String>>) -> std::result::Result<i64, String> {
        crate::jobs::start(&self.jobs, job_id, "ai", only).map(|r| r.id).map_err(|e| e.message)
    }
    async fn approve(&self, tool: &str, summary: &str, detail: &Value) -> Option<Value> {
        let r = self
            .ai
            .ask(
                self.ui.as_ref(),
                "approval_request",
                json!({"run_id": self.run_id, "tool": tool, "summary": summary, "detail": detail}),
                Duration::from_secs(600),
            )
            .await?;
        r.get("approved").and_then(|a| a.as_bool()).filter(|a| *a).map(|_| r.get("detail").cloned().unwrap_or_else(|| detail.clone()))
    }
    async fn editor_state(&self) -> Option<Value> {
        self.ai.ask(self.ui.as_ref(), "editor_request", json!({"run_id": self.run_id, "tab_id": self.tab_id}), Duration::from_secs(10)).await
    }
    async fn propose_edit(&self, proposal: &Value) -> bool {
        self.ai
            .ask(self.ui.as_ref(), "edit_proposal", json!({"run_id": self.run_id, "tab_id": self.tab_id, "proposal": proposal}), Duration::from_secs(600))
            .await
            .and_then(|r| r.get("accepted").and_then(|a| a.as_bool()))
            .unwrap_or(false)
    }
    fn current_result(&self) -> Option<String> {
        self.result_id.clone()
    }
}

#[derive(Debug, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct AiSendArgs {
    #[serde(flatten)]
    pub request: AgentRequest,
    #[serde(default)]
    pub tab_id: Option<String>,
}

/// Start an agent turn; progress arrives as `ai-event`s. Returns the run id.
pub fn ai_send(state: &AppState, args: AiSendArgs) -> Result<String> {
    let run_id = uuid::Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    state.ai.runs.lock().insert(run_id.clone(), cancel.clone());
    let agent = Agent { engine: state.engine.clone(), hub: state.hub.clone(), secrets: state.secrets.clone(), max_steps: 12 };
    let host = Arc::new(UiHost {
        ui: state.ui.clone(),
        ai: state.ai.clone(),
        run_id: run_id.clone(),
        result_id: args.request.context.result_id.clone(),
        tab_id: args.tab_id.clone(),
        jobs: state.job_ctx(),
    });
    let sink = Arc::new(UiSink(state.ui.clone()));
    let runs = state.ai.runs.clone();
    let rid = run_id.clone();
    let session_id = args.request.session_id.clone().unwrap_or_default();
    tokio::spawn(async move {
        if let Err(error) = agent.run(args.request, rid.clone(), host, sink.clone(), cancel).await {
            sink.emit(AgentEvent::Failed { session_id, run_id: rid.clone(), error });
        }
        runs.lock().remove(&rid);
    });
    Ok(run_id)
}

/// Inline completion at the editor's cursor. `request_id` (chosen by the
/// editor) cancels it with [`ai_cancel`] when the user types on.
pub async fn ai_ghost(state: &AppState, request_id: String, request: databrain_ai::ghost::GhostRequest) -> Result<String> {
    let cancel = CancellationToken::new();
    state.ai.runs.lock().insert(request_id.clone(), cancel.clone());
    let r = databrain_ai::ghost::complete(&state.workspace, state.secrets.clone(), &request, &cancel).await;
    state.ai.runs.lock().remove(&request_id);
    r.map_err(ai_err)
}

/// Rows the AI writes for "Generate test data".
pub async fn ai_test_data(
    state: &AppState,
    connection_id: &str,
    ai: &databrain_ai::testdata::TestDataAi,
    ask: &databrain_query_engine::testdata::AiAsk,
    cancel: &CancellationToken,
) -> Result<databrain_query_engine::testdata::AiSample> {
    databrain_ai::testdata::sample(&state.workspace, state.secrets.clone(), connection_id, ai, ask, cancel).await.map_err(ai_err)
}

/// Patterns the AI suggests for "Generate test data" (unchecked).
pub async fn ai_test_data_patterns(
    state: &AppState,
    connection_id: &str,
    ai: &databrain_ai::testdata::TestDataAi,
    ask: &databrain_query_engine::testdata::AiAsk,
    cancel: &CancellationToken,
) -> Result<std::collections::HashMap<String, String>> {
    databrain_ai::testdata::suggest_patterns(&state.workspace, state.secrets.clone(), connection_id, ai, ask, cancel).await.map_err(ai_err)
}

/// `cancel` runs under `id` for [`ai_cancel`] until the guard is dropped.
pub fn track_run(state: &AppState, id: String, cancel: CancellationToken) -> RunGuard<'_> {
    state.ai.runs.lock().insert(id.clone(), cancel);
    RunGuard { state, id }
}

pub struct RunGuard<'a> {
    state: &'a AppState,
    id: String,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.state.ai.runs.lock().remove(&self.id);
    }
}

pub fn ai_cancel(state: &AppState, run_id: &str) -> bool {
    match state.ai.runs.lock().get(run_id) {
        Some(c) => {
            c.cancel();
            true
        }
        None => false,
    }
}

/// Answer a pending UI request (approval, edit proposal, editor state).
pub fn ai_respond(state: &AppState, request_id: &str, response: Value) -> bool {
    match state.ai.pending.lock().remove(request_id) {
        Some(tx) => tx.send(response).is_ok(),
        None => false,
    }
}

pub fn list_sessions(state: &AppState, connection_id: Option<String>) -> Result<Vec<AiSessionRecord>> {
    Ok(state.workspace.list_ai_sessions(connection_id.as_deref(), 100)?)
}

pub fn session_messages(state: &AppState, session_id: &str) -> Result<Vec<AiMessageRecord>> {
    Ok(state.workspace.list_ai_messages(session_id)?)
}

/// A chat closed in the panel (still in history): its outputs stop being
/// active, so the memory budget can free them; they stay listed.
pub fn leave_session(state: &AppState, id: &str) {
    for part in databrain_workspace::AiPart::ALL {
        state.engine.leave_tab(&databrain_workspace::SessionRole::Ai { chat: id.to_string(), part }.key());
    }
}

pub fn delete_session(state: &AppState, id: &str) -> Result<()> {
    // The chat's query tabs: their sessions and the outputs they kept active.
    for part in databrain_workspace::AiPart::ALL {
        state.engine.close_tab(&databrain_workspace::SessionRole::Ai { chat: id.to_string(), part }.key());
    }
    Ok(state.workspace.delete_ai_session(id)?)
}

pub fn list_audit(state: &AppState) -> Result<Vec<AuditEntry>> {
    Ok(state.workspace.list_audit(200)?)
}

// ------------------------------------------------------------------ knowledge

#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct KnowledgeView {
    pub state: Option<KnState>,
    pub notes: Vec<KnNote>,
}

/// Index state and notes. Objects are paged with [`knowledge_objects`]
/// (an index can hold 10k+ tables).
pub fn knowledge(state: &AppState, connection_id: &str) -> Result<KnowledgeView> {
    // The count is what the AI may use now (scope edits and listings change
    // it between runs), not what the last run saw.
    let objects = databrain_ai::catalog::AiTables::new(&state.workspace, connection_id).and_then(|t| t.count()).map_err(ai_err)?;
    let st = state.workspace.kn_state(connection_id)?.map(|s| databrain_workspace::KnState { objects, ..s });
    Ok(KnowledgeView { state: st, notes: state.workspace.kn_notes(connection_id)? })
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
pub struct KnObjectPage {
    pub objects: Vec<KnObject>,
    /// Objects matching the filter (all pages).
    pub total: usize,
}

/// The tables the AI may use (Knowledge → Index), a page at a time.
pub fn knowledge_objects(state: &AppState, connection_id: &str, filter: &str, offset: usize, limit: usize) -> Result<KnObjectPage> {
    let tables = databrain_ai::catalog::AiTables::new(&state.workspace, connection_id).map_err(ai_err)?;
    let (objects, total) = tables.page(filter, offset, limit.clamp(1, 1000)).map_err(ai_err)?;
    Ok(KnObjectPage { objects, total })
}

/// What indexing would cover (schemas, catalogs, table counts) so the UI
/// can ask before a large run. Uses one schema listing, no per-schema queries.
pub async fn knowledge_plan(state: &AppState, connection_id: &str) -> Result<databrain_ai::knowledge::IndexPlan> {
    databrain_ai::knowledge::plan(&state.engine, connection_id).await.map_err(ai_err)
}

/// Index in the background; progress/completion via `knowledge-event`.
///
/// `scope` (schema ids, `catalog.*`, or `*`) limits the run; it is saved as
/// the connection's `ai_policy.index_schemas` so re-indexing uses it.
/// Index in the background. Only schemas whose definitions changed since the
/// last run are re-read, unless `full`.
pub fn index_knowledge(state: &AppState, connection_id: &str, scope: Option<Vec<String>>, batch: Option<u32>, full: bool) -> Result<()> {
    if scope.is_some() || batch.is_some() {
        let mut p = state.workspace.get_connection(connection_id)?;
        if let Some(sc) = &scope {
            if sc.is_empty() {
                return Err(EngineError::new("invalid", "choose at least one catalog or schema to index"));
            }
            p.ai_policy.index_schemas = if sc.iter().any(|s| s.trim() == "*") { vec!["*".into()] } else { sc.clone() };
            // The scope is what the AI may use. `*` is carried by the rule
            // each indexed schema gets (system schemas never are): the rules
            // there are kept until the run (re)allows each schema, so a
            // cancelled or failed run takes nothing away.
            if !sc.iter().any(|s| s.trim() == "*") {
                let named: Vec<String> = sc.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                state.workspace.ai_set_schema_rules(connection_id, &named).map_err(EngineError::from)?;
            }
        }
        if let Some(b) = batch {
            p.ai_policy.index_batch = b.clamp(1, databrain_ai::knowledge::MAX_BATCH as u32);
        }
        state.workspace.save_connection(p)?;
    }
    static RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let run = RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cancel = CancellationToken::new();
    {
        let mut runs = state.ai.indexing.lock();
        if runs.get(connection_id).is_some_and(|(_, c)| !c.is_cancelled()) {
            return Err(EngineError::new("busy", "this connection is already being indexed"));
        }
        runs.insert(connection_id.to_string(), (run, cancel.clone()));
    }
    let engine = state.engine.clone();
    let ui = state.ui.clone();
    let runs = state.ai.indexing.clone();
    let cid = connection_id.to_string();
    tokio::spawn(async move {
        let ui2 = ui.clone();
        let c2 = cid.clone();
        let progress = move |schema: &str, done: usize, total: usize| {
            ui2.emit(KNOWLEDGE_EVENT, json!({"type": "progress", "connection_id": c2, "schema": schema, "done": done, "total": total}));
        };
        let r = databrain_ai::knowledge::index_connection_with(&engine, &cid, None, full, &progress, &cancel).await;
        {
            let mut m = runs.lock();
            if m.get(&cid).is_some_and(|(r, _)| *r == run) {
                m.remove(&cid);
            }
        }
        // Tables known now may place joins that couldn't be placed before.
        let relearn = r.is_ok() && engine.workspace().get_connection(&cid).is_ok_and(|p| p.ai_policy.learn_from_history && learn::supported(p.config.kind));
        if relearn {
            let ws = engine.workspace().clone();
            let id = cid.clone();
            let learned = tokio::task::spawn_blocking(move || {
                let _one = LEARNING.lock();
                learn::relearn(&ws, &id)
            })
            .await;
            if let Ok(Ok(report)) = learned {
                ui.emit(KNOWLEDGE_EVENT, json!({"type": "learned", "connection_id": cid, "report": report}));
            }
        }
        ui.emit(
            KNOWLEDGE_EVENT,
            match r {
                Ok(report) => json!({"type": "finished", "connection_id": cid, "report": report}),
                Err(databrain_ai::AiError::Cancelled) => json!({"type": "cancelled", "connection_id": cid}),
                Err(e) => json!({"type": "failed", "connection_id": cid, "error": e.to_string()}),
            },
        );
    });
    Ok(())
}

/// Stop a running index; schemas finished so far are kept.
pub fn cancel_index(state: &AppState, connection_id: &str) -> bool {
    match state.ai.indexing.lock().get(connection_id) {
        Some((_, c)) => {
            c.cancel();
            true
        }
        None => false,
    }
}

/// Save a note. Its target must name tables (columns) of the connection; it
/// is stored with canonical names.
pub async fn save_note(state: &AppState, mut note: KnNote) -> Result<KnNote> {
    if let Some(t) = note.target.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let canon = databrain_ai::knowledge::check_note_target(&state.engine, &note.connection_id, t).await.map_err(|e| EngineError::new("invalid", e))?;
        note.target = Some(canon);
    }
    Ok(state.workspace.kn_save_note(note)?)
}

/// Layout of a table referenced by an executed query (for query tips).
#[derive(Debug, Serialize)]
pub struct HintLayout {
    /// The reference as written (`sales.orders`).
    pub written: String,
    pub schema: Option<String>,
    pub name: Option<String>,
    pub layout: Option<databrain_connector_core::TableLayout>,
    pub error: Option<String>,
}

/// Resolve the tables a query used and fetch their indexes / partitions /
/// cluster keys. Errors are reported per table.
pub async fn hint_layouts(state: &AppState, connection_id: &str, tables: Vec<String>) -> Result<Vec<HintLayout>> {
    let mut out = Vec::new();
    for written in tables.into_iter().take(30) {
        let mut h = HintLayout { written: written.clone(), schema: None, name: None, layout: None, error: None };
        match databrain_ai::knowledge::resolve_table(&state.engine, connection_id, &written).await {
            Ok(Some((schema, name))) => {
                match state.engine.table_layout(connection_id, &schema, &name).await {
                    Ok(l) => h.layout = Some(l),
                    Err(e) => h.error = Some(e.message),
                }
                h.schema = Some(schema);
                h.name = Some(name);
            }
            Ok(None) => h.error = Some("table not found".into()),
            Err(e) => h.error = Some(e),
        }
        out.push(h);
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct TargetCheck {
    pub ok: bool,
    /// Canonical target when ok.
    pub target: Option<String>,
    pub error: Option<String>,
}

pub async fn check_target(state: &AppState, connection_id: &str, target: &str) -> TargetCheck {
    match databrain_ai::knowledge::check_note_target(&state.engine, connection_id, target).await {
        Ok(t) => TargetCheck { ok: true, target: Some(t), error: None },
        Err(e) => TargetCheck { ok: false, target: None, error: Some(e) },
    }
}

pub fn delete_note(state: &AppState, id: &str) -> Result<()> {
    Ok(state.workspace.kn_delete_note(id)?)
}

/// Write the connection's approved notes & glossary to `path` (JSON).
pub fn export_notes(state: &AppState, connection_id: &str, path: &str) -> Result<usize> {
    let file = state.workspace.kn_export_notes(connection_id)?;
    let text = serde_json::to_string_pretty(&file).map_err(|e| EngineError::new("internal", e.to_string()))?;
    std::fs::write(path, text).map_err(|e| EngineError::new("io", format!("cannot write {path}: {e}")))?;
    Ok(file.notes.len())
}

#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct NotesImportPreview {
    pub source: Option<databrain_workspace::NotesSource>,
    pub items: Vec<databrain_workspace::ImportItem>,
}

/// Read a notes file and compare it with the connection's notes.
pub async fn read_notes_file(state: &AppState, connection_id: &str, path: &str) -> Result<NotesImportPreview> {
    let meta = std::fs::metadata(path).map_err(|e| EngineError::new("io", format!("cannot read {path}: {e}")))?;
    if meta.len() > 50 * 1024 * 1024 {
        return Err(EngineError::new("invalid", "notes file is larger than 50 MB"));
    }
    let text = std::fs::read_to_string(path).map_err(|e| EngineError::new("io", format!("cannot read {path}: {e}")))?;
    let file: databrain_workspace::NotesFile =
        serde_json::from_str(&text).map_err(|e| EngineError::new("invalid", format!("not a DataBrain notes file: {e}")))?;
    let mut items = state.workspace.kn_import_plan(connection_id, &file)?;
    // Tables the notes are about must exist here (checked once per target).
    let mut seen: HashMap<String, Option<String>> = HashMap::new();
    for it in items.iter_mut() {
        let Some(t) = it.incoming.target.clone().filter(|t| !t.trim().is_empty()) else { continue };
        if !seen.contains_key(&t) {
            let r = if seen.len() < 500 { databrain_ai::knowledge::check_note_target(&state.engine, connection_id, &t).await.err() } else { None };
            seen.insert(t.clone(), r);
        }
        it.invalid_target = seen[&t].clone();
    }
    Ok(NotesImportPreview { source: file.source, items })
}

pub async fn import_notes(state: &AppState, connection_id: &str, mut actions: Vec<databrain_workspace::ImportAction>) -> Result<usize> {
    state.workspace.get_connection(connection_id)?;
    let mut bad = Vec::new();
    for a in actions.iter_mut().filter(|a| a.action != "skip") {
        let Some(t) = a.target.clone().or_else(|| a.incoming.target.clone()).filter(|t| !t.trim().is_empty()) else { continue };
        match databrain_ai::knowledge::check_note_target(&state.engine, connection_id, &t).await {
            Ok(canon) => a.target = Some(canon),
            Err(e) => bad.push(format!("{t}: {e}")),
        }
    }
    if !bad.is_empty() {
        return Err(EngineError::new("invalid", format!("some notes are about tables this connection does not have: {}", bad.join("; "))));
    }
    Ok(state.workspace.kn_import_apply(connection_id, &actions)?)
}

// ------------------------------------------------- learned from query history

/// One learner at a time (the periodic check, "learn now", after indexing):
/// two would read the same history entries.
static LEARNING: Mutex<()> = parking_lot::const_mutex(());

/// Passes per connection in one periodic check (500 entries each).
const LEARN_PASSES_PER_CHECK: usize = 20;

/// Learn from new history on every connection that has some and learns
/// (blocking; run off the async runtime). Returns what changed.
pub fn learn_new_history(ws: &databrain_workspace::Workspace) -> Vec<(String, LearnReport)> {
    let _one = LEARNING.lock();
    let mut out = Vec::new();
    for id in ws.connections_to_learn().unwrap_or_default() {
        if !ws.get_connection(&id).is_ok_and(|p| p.ai_policy.learn_from_history) {
            continue;
        }
        let mut total = LearnReport::default();
        for _ in 0..LEARN_PASSES_PER_CHECK {
            match learn::learn_pass(ws, &id, learn::PASS_LIMIT) {
                Ok(r) => {
                    total.entries += r.entries;
                    total.joins += r.joins;
                    total.tables += r.tables;
                    total.last_history_id = r.last_history_id;
                    total.done = r.done;
                    if r.done {
                        break;
                    }
                }
                Err(e) => {
                    let at = ws.learn_state(&id).ok().flatten().map(|s| s.last_history_id).unwrap_or(0);
                    let _ = ws.set_learn_state(&id, at, Some(&e.to_string()));
                    break;
                }
            }
        }
        out.push((id, total));
    }
    out
}

/// Every minute (and at start): learn from new history; tell the UI when
/// something was learned.
pub async fn learn_loop(ws: Arc<databrain_workspace::Workspace>, ui: Arc<dyn UiBridge>) {
    loop {
        let w = ws.clone();
        if let Ok(reports) = tokio::task::spawn_blocking(move || learn_new_history(&w)).await {
            for (id, r) in reports.into_iter().filter(|(_, r)| r.joins > 0 || r.tables > 0) {
                ui.emit(KNOWLEDGE_EVENT, json!({"type": "learned", "connection_id": id, "report": r}));
            }
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// Learn again from all kept history of a connection (after indexing, or
/// "Learn now").
pub async fn relearn(state: &AppState, connection_id: &str) -> Result<LearnReport> {
    let ws = state.workspace.clone();
    let id = connection_id.to_string();
    let p = ws.get_connection(&id)?;
    if !learn::supported(p.config.kind) {
        return Err(EngineError::new("invalid", format!("{} queries can't be learned from", p.config.kind.traits().name)));
    }
    if !p.ai_policy.learn_from_history {
        return Err(EngineError::new("invalid", "learning from query history is off for this connection (Edit connection → AI)"));
    }
    let r = tokio::task::spawn_blocking(move || {
        let _one = LEARNING.lock();
        learn::relearn(&ws, &id)
    })
    .await
    .map_err(|e| EngineError::new("internal", e.to_string()))?
    .map_err(ai_err)?;
    state.ui.emit(KNOWLEDGE_EVENT, json!({"type": "learned", "connection_id": connection_id, "report": r}));
    Ok(r)
}

/// A learned join and whether the AI is given it now.
#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LearnedJoinView {
    #[serde(flatten)]
    pub join: LearnedJoin,
    /// In the AI's schema context now.
    pub shown: bool,
    /// Why not, when it isn't.
    pub hidden_reason: Option<String>,
}

#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LearnedView {
    /// History of this kind of database can be learned from.
    pub supported: bool,
    /// The connection's "Learn from query history" setting.
    pub enabled: bool,
    pub state: Option<LearnState>,
    pub joins: Vec<LearnedJoinView>,
    /// Most used tables (top 20).
    pub tables: Vec<TableUsage>,
}

pub fn learned(state: &AppState, connection_id: &str) -> Result<LearnedView> {
    let ws = &state.workspace;
    let p = ws.get_connection(connection_id)?;
    let pol = &p.ai_policy;
    let usable: std::collections::HashSet<i64> = databrain_ai::knowledge::usable_joins(ws, &p).map_err(ai_err)?.into_iter().map(|j| j.id).collect();
    let readable = |t: &str| databrain_ai::access::check_read_table(&p, t).is_ok();
    let joins = ws
        .learned_joins(connection_id)?
        .into_iter()
        .map(|j| {
            let shown = usable.contains(&j.id);
            let proposed = j.documented.as_ref().is_some_and(|d| d.state == databrain_workspace::DocState::Proposed);
            let hidden_reason = (!shown).then(|| {
                if !pol.share_metadata {
                    "Schema metadata isn't shared with the AI on this connection".to_string()
                } else if j.status == JoinStatus::Rejected {
                    "Rejected".into()
                } else if proposed {
                    "Recorded by the AI from a document: approve it to give it to the AI".into()
                } else if !pol.learn_from_history && !learn::documented(&j) {
                    "Learning from query history is off for this connection".into()
                } else if j.declared {
                    "A declared foreign key: the AI sees it as FK".into()
                } else if !learn::surfaced(&j) {
                    if j.queries == 0 {
                        "Only the AI's own queries used it: it counts once you or a job run it".into()
                    } else {
                        "Seen in one query on one day: given to the AI after another query or another day".into()
                    }
                } else if !readable(&j.left_table) || !readable(&j.right_table) {
                    "Joins a table the AI may not read (table access rules)".into()
                } else {
                    "A column it uses no longer exists in the indexed table".into()
                }
            });
            LearnedJoinView { join: j, shown, hidden_reason }
        })
        .collect();
    let mut tables = ws.table_usage(connection_id)?;
    tables.truncate(20);
    Ok(LearnedView { supported: learn::supported(p.config.kind), enabled: pol.learn_from_history, state: ws.learn_state(connection_id)?, joins, tables })
}

pub fn set_join_status(state: &AppState, id: i64, status: JoinStatus) -> Result<()> {
    Ok(state.workspace.set_join_status(id, status)?)
}

/// Approve a join the AI recorded from a document, or discard it (what
/// queries taught about it stays).
pub fn review_join(state: &AppState, id: i64, approve: bool) -> Result<()> {
    Ok(state.workspace.review_documented_join(id, approve)?)
}

/// History entries a learned join was seen in (still kept), latest first.
pub fn join_evidence(state: &AppState, join_id: i64) -> Result<Vec<databrain_workspace::HistoryEntry>> {
    Ok(state.workspace.join_evidence(join_id, 10)?)
}

/// Forget what was learned on a connection; the history already there is
/// not learned from again.
pub fn forget_learned(state: &AppState, connection_id: &str) -> Result<()> {
    Ok(state.workspace.forget_learned(Some(connection_id))?)
}

pub fn clear_knowledge(state: &AppState, connection_id: &str) -> Result<()> {
    Ok(state.workspace.kn_clear(connection_id)?)
}

/// Snippet for configuring external MCP clients (Kiro CLI, Claude Code...).
pub fn mcp_config() -> Value {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(if cfg!(windows) { "databrain-mcp.exe" } else { "databrain-mcp" })))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "databrain-mcp".into());
    json!({"mcpServers": {"databrain": {"command": exe, "args": []}}})
}
