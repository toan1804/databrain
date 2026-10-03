//! AI mode, knowledge and sign-in API used by Tauri commands.
//!
//! The UI is reached through [`UiBridge`] (Tauri events in the app, a
//! recorder in tests). Approvals and editor proposals are request/response:
//! the backend emits a request with an id and waits for the UI to answer via
//! `ai_respond`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use databrain_ai::{Agent, AgentEvent, AgentRequest, AgentSink, ModelInfo, ProviderAuth, ProviderConfig, ProviderKind, ToolHost};
use databrain_auth::{AuthError, AuthStatus, CancellationToken, DeviceCodePrompt, Interaction, SecretRef, SecretString};
use databrain_query_engine::{EngineError, Result};
use databrain_workspace::{AiMessageRecord, AiProviderRecord, AiSessionRecord, AuditEntry, KnNote, KnObject, KnState};
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
        let mut p = payload;
        p["type"] = kind.into();
        p["request_id"] = id.clone().into();
        ui.emit(AI_EVENT, p);
        let r = tokio::time::timeout(timeout, rx).await.ok().and_then(|r| r.ok());
        self.pending.lock().remove(&id);
        r
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

#[derive(Debug, Serialize)]
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
        .map(|r| ProviderView {
            has_key: state.secrets.get(&SecretRef::for_ai_provider(&r.id)).ok().flatten().is_some(),
            record: r,
        })
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
            kiro.sign_in_browser(cancel, || ui.emit(AUTH_EVENT, json!({"type": "terminal_opened", "command": "kiro-cli login"})))
                .await
                .map_err(ai_err)?;
            state.ui.emit(AUTH_EVENT, json!({"type": "finished"}));
            cfg.auth = ProviderAuth::KiroBrowser;
        }
        _ => return Err(EngineError::new("invalid", "browser sign-in is available for OpenRouter and Kiro; other providers use API keys")),
    }
    rec.config = serde_json::to_value(cfg).unwrap_or_default();
    state.workspace.save_ai_provider(rec)?;
    Ok(())
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
}

#[async_trait]
impl ToolHost for UiHost {
    async fn approve(&self, tool: &str, summary: &str, detail: &Value) -> Option<Value> {
        let r = self
            .ai
            .ask(self.ui.as_ref(), "approval_request", json!({"run_id": self.run_id, "tool": tool, "summary": summary, "detail": detail}), Duration::from_secs(600))
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

#[derive(Debug, Deserialize)]
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

pub fn delete_session(state: &AppState, id: &str) -> Result<()> {
    Ok(state.workspace.delete_ai_session(id)?)
}

pub fn list_audit(state: &AppState) -> Result<Vec<AuditEntry>> {
    Ok(state.workspace.list_audit(200)?)
}

// ------------------------------------------------------------------ knowledge

#[derive(Debug, Serialize)]
pub struct KnowledgeView {
    pub state: Option<KnState>,
    pub objects: Vec<KnObject>,
    pub notes: Vec<KnNote>,
}

pub fn knowledge(state: &AppState, connection_id: &str) -> Result<KnowledgeView> {
    Ok(KnowledgeView {
        state: state.workspace.kn_state(connection_id)?,
        objects: state.workspace.kn_objects(connection_id)?,
        notes: state.workspace.kn_notes(connection_id)?,
    })
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
pub fn index_knowledge(state: &AppState, connection_id: &str, scope: Option<Vec<String>>, batch: Option<u32>) -> Result<()> {
    if scope.is_some() || batch.is_some() {
        let mut p = state.workspace.get_connection(connection_id)?;
        if let Some(sc) = &scope {
            if sc.is_empty() {
                return Err(EngineError::new("invalid", "choose at least one catalog or schema to index"));
            }
            p.ai_policy.index_schemas = if sc.iter().any(|s| s.trim() == "*") { vec!["*".into()] } else { sc.clone() };
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
        let r = databrain_ai::knowledge::index_connection(&engine, &cid, None, &progress, &cancel).await;
        {
            let mut m = runs.lock();
            if m.get(&cid).is_some_and(|(r, _)| *r == run) {
                m.remove(&cid);
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

pub fn save_note(state: &AppState, note: KnNote) -> Result<KnNote> {
    Ok(state.workspace.kn_save_note(note)?)
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

#[derive(Debug, Serialize)]
pub struct NotesImportPreview {
    pub source: Option<databrain_workspace::NotesSource>,
    pub items: Vec<databrain_workspace::ImportItem>,
}

/// Read a notes file and compare it with the connection's notes.
pub fn read_notes_file(state: &AppState, connection_id: &str, path: &str) -> Result<NotesImportPreview> {
    let meta = std::fs::metadata(path).map_err(|e| EngineError::new("io", format!("cannot read {path}: {e}")))?;
    if meta.len() > 50 * 1024 * 1024 {
        return Err(EngineError::new("invalid", "notes file is larger than 50 MB"));
    }
    let text = std::fs::read_to_string(path).map_err(|e| EngineError::new("io", format!("cannot read {path}: {e}")))?;
    let file: databrain_workspace::NotesFile =
        serde_json::from_str(&text).map_err(|e| EngineError::new("invalid", format!("not a DataBrain notes file: {e}")))?;
    let items = state.workspace.kn_import_plan(connection_id, &file)?;
    Ok(NotesImportPreview { source: file.source, items })
}

pub fn import_notes(state: &AppState, connection_id: &str, actions: Vec<databrain_workspace::ImportAction>) -> Result<usize> {
    state.workspace.get_connection(connection_id)?;
    Ok(state.workspace.kn_import_apply(connection_id, &actions)?)
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
