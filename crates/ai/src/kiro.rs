//! Kiro as an AI provider.
//!
//! Kiro has no public model API; it is used through the local `kiro-cli`:
//!
//! - **Auth.** Either the CLI's own browser sign-in (`kiro-cli login`:
//!   Builder ID, GitHub, Google, IAM Identity Center) or a Kiro API key
//!   (`ksk_…`, Pro plans) that DataBrain keeps in the OS keychain and passes
//!   to the child process as `KIRO_API_KEY` (never written to disk).
//! - **Chat.** Each turn spawns `kiro-cli acp` (Agent Client Protocol,
//!   JSON-RPC over stdio) with a DataBrain-managed agent config whose tools
//!   are limited to `@databrain`. DataBrain's tools (schema search, run_query
//!   with approvals, editor proposals …) are served to Kiro through an MCP
//!   endpoint that exists only for the turn: bound to 127.0.0.1, random port,
//!   random bearer token, Origin/Host checked. Tool calls go through the same
//!   [`ToolContext`] and policy engine as the built-in agent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentEvent, AgentSink};
use crate::providers::{KeySource, LlmProvider, ProviderAuth, ProviderConfig, ProviderKind};
use crate::tools::{ToolContext, specs};
use crate::types::*;

/// Name of the Kiro agent config DataBrain manages (`~/.kiro/agents/`).
pub const AGENT_NAME: &str = "databrain-sql";
/// MCP server name Kiro sees; tools appear as `@databrain/<tool>`.
pub const MCP_NAME: &str = "databrain";
const MAX_TOOL_CONTENT: usize = 16_000;

// ------------------------------------------------------------------ provider

pub struct KiroProvider {
    cli_override: Option<String>,
    agents_dir: Option<PathBuf>,
    /// Extra environment for kiro-cli (never contains KIRO_API_KEY).
    env: std::collections::BTreeMap<String, String>,
    key: KeySource,
    browser: bool,
}

/// Sign-in state reported by `kiro-cli whoami`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct KiroStatus {
    pub installed: bool,
    pub cli_path: Option<String>,
    pub signed_in: bool,
    /// `ApiKey`, `BuilderId`, `IdentityCenter`, social …
    pub account_type: Option<String>,
    pub identity: Option<String>,
    pub message: Option<String>,
}

impl KiroProvider {
    pub fn new(cfg: &ProviderConfig, key: KeySource) -> Result<Self> {
        Ok(Self {
            cli_override: cfg.base_url.clone().filter(|p| !p.trim().is_empty()),
            agents_dir: cfg.agents_dir.as_deref().filter(|d| !d.trim().is_empty()).map(expand_home),
            env: cfg.cli_env.clone(),
            key,
            browser: cfg.auth != ProviderAuth::ApiKey,
        })
    }

    /// Use a different directory for the managed agent config (tests).
    pub fn with_agents_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.agents_dir = Some(dir.into());
        self
    }

    /// Locate `kiro-cli`. GUI apps don't inherit the shell PATH, so the usual
    /// install locations are checked too.
    pub fn cli_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.cli_override {
            let p = expand_home(p);
            return p.is_file().then_some(p);
        }
        find_cli()
    }

    fn agents_dir(&self) -> Option<PathBuf> {
        self.agents_dir.clone().or_else(|| home().map(|h| h.join(".kiro").join("agents")))
    }

    /// Command with the auth environment for the selected mode.
    async fn command(&self) -> Result<Command> {
        let program = self.cli_path().ok_or_else(not_installed)?;
        let mut cmd = Command::new(program);
        cmd.envs(self.env.iter().filter(|(k, _)| *k != "KIRO_API_KEY"));
        cmd.env_remove("KIRO_API_KEY");
        if !self.browser {
            let key = self.key.get().await?.ok_or_else(|| AiError::Auth("add your Kiro API key (ksk_…) in Settings → AI providers".into()))?;
            cmd.env("KIRO_API_KEY", key);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        Ok(cmd)
    }

    pub async fn status(&self) -> KiroStatus {
        let Some(path) = self.cli_path() else {
            return KiroStatus {
                installed: false,
                cli_path: None,
                signed_in: false,
                account_type: None,
                identity: None,
                message: Some(not_installed().to_string()),
            };
        };
        let base = KiroStatus { installed: true, cli_path: Some(path.to_string_lossy().into()), signed_in: false, account_type: None, identity: None, message: None };
        let mut cmd = match self.command().await {
            Ok(c) => c,
            Err(e) => return KiroStatus { message: Some(e.to_string()), ..base },
        };
        cmd.args(["whoami", "--format", "json"]);
        match tokio::time::timeout(Duration::from_secs(20), cmd.output()).await {
            Ok(Ok(out)) => {
                let (signed_in, account_type, identity) = parse_whoami(&String::from_utf8_lossy(&out.stdout));
                let message = (!signed_in).then(|| {
                    if self.browser { "Not signed in. Use “Sign in with browser”.".to_string() } else { "Kiro rejected the API key.".to_string() }
                });
                KiroStatus { signed_in, account_type, identity, message, ..base }
            }
            Ok(Err(e)) => KiroStatus { message: Some(format!("cannot run kiro-cli: {e}")), ..base },
            Err(_) => KiroStatus { message: Some("kiro-cli whoami timed out".into()), ..base },
        }
    }

    /// Browser sign-in: `kiro-cli login` needs a terminal for the provider
    /// menu, so it runs in a terminal window; the browser opens from there.
    /// Completes when `whoami` reports a session.
    pub async fn sign_in_browser(&self, cancel: CancellationToken, on_started: impl FnOnce()) -> Result<KiroStatus> {
        let program = self.cli_path().ok_or_else(not_installed)?;
        let browser_only = KiroProvider {
            cli_override: Some(program.to_string_lossy().into()),
            agents_dir: None,
            env: self.env.clone(),
            key: KeySource::None,
            browser: true,
        };
        let st = browser_only.status().await;
        if st.signed_in {
            return Ok(st);
        }
        open_login_terminal(&program)?;
        on_started();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(900);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Err(AiError::Cancelled),
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            let st = browser_only.status().await;
            if st.signed_in {
                return Ok(st);
            }
            if tokio::time::Instant::now() > deadline {
                return Err(AiError::Auth("Kiro sign-in timed out".into()));
            }
        }
    }

    /// `kiro-cli logout` (ends the CLI's browser session on this computer).
    pub async fn sign_out(&self) -> Result<()> {
        let program = self.cli_path().ok_or_else(not_installed)?;
        let out = Command::new(program).arg("logout").env_remove("KIRO_API_KEY").stdin(Stdio::null()).output().await.map_err(|e| AiError::Provider(e.to_string()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.to_lowercase().contains("not logged in") {
                return Err(AiError::Provider(format!("kiro-cli logout failed: {}", err.trim())));
            }
        }
        Ok(())
    }

    /// Write/refresh the managed agent config (tools limited to DataBrain's).
    fn ensure_agent(&self) -> Result<()> {
        let dir = self.agents_dir().ok_or_else(|| AiError::Config("cannot find the home directory for ~/.kiro/agents".into()))?;
        std::fs::create_dir_all(&dir).map_err(|e| AiError::Config(format!("cannot create {}: {e}", dir.display())))?;
        let cfg = json!({
            "name": AGENT_NAME,
            "description": "DataBrain SQL assistant. Managed by DataBrain — tools are limited to the DataBrain MCP server.",
            "tools": [format!("@{MCP_NAME}")],
            "allowedTools": [format!("@{MCP_NAME}")],
            "includeMcpJson": false,
            "includePowers": false,
            "resources": [],
            "prompt": "You are DataBrain's SQL assistant inside a desktop SQL client. Use only the @databrain tools to inspect \
                       the database, run queries and write to the user's editor. Each message starts with a [DataBrain context] \
                       block describing the connection, rules and relevant schema; follow it."
        });
        let path = dir.join(format!("{AGENT_NAME}.json"));
        let text = serde_json::to_string_pretty(&cfg).unwrap_or_default();
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&path, text).map_err(|e| AiError::Config(format!("cannot write {}: {e}", path.display())))?;
        }
        Ok(())
    }

    async fn spawn_acp(&self, model: Option<&str>) -> Result<(Acp, mpsc::UnboundedReceiver<Value>)> {
        self.ensure_agent()?;
        let mut cmd = self.command().await?;
        cmd.args(["acp", "--agent", AGENT_NAME]);
        if let Some(m) = model.filter(|m| !m.is_empty() && *m != "auto") {
            cmd.args(["--model", m]);
        }
        let cwd = std::env::temp_dir().join("databrain-kiro");
        let _ = std::fs::create_dir_all(&cwd);
        cmd.current_dir(&cwd).stdin(Stdio::piped());
        Acp::spawn(cmd, cwd).await
    }

    /// One agent turn. `prompt` already contains DataBrain's context block.
    pub(crate) async fn run_turn(&self, t: KiroTurn) -> Result<String> {
        let st = self.status().await;
        if !st.signed_in {
            return Err(AiError::Auth(format!(
                "Kiro: {}",
                st.message.unwrap_or_else(|| "not signed in".into())
            )));
        }
        let mcp = McpHttp::start(t.ctx.clone(), t.sink.clone(), t.session_id.clone(), t.run_id.clone()).await?;
        let (acp, mut incoming) = self.spawn_acp(t.model.as_deref()).await?;
        let init = json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "databrain", "version": env!("CARGO_PKG_VERSION")}
        });
        acp.call(&mut incoming, "initialize", init, &t.cancel, |_| {}).await?;
        let servers = json!([mcp.server_entry()]);
        let cwd = acp.cwd.to_string_lossy().to_string();

        // Continue the Kiro session of this DataBrain conversation when possible.
        let mut kiro_session = None;
        if let Some(prev) = &t.previous_session {
            let p = json!({"sessionId": prev, "cwd": cwd, "mcpServers": servers});
            if acp.call(&mut incoming, "session/load", p, &t.cancel, |_| {}).await.is_ok() {
                kiro_session = Some(prev.clone());
            }
        }
        let kiro_session = match kiro_session {
            Some(s) => s,
            None => {
                let r = acp.call(&mut incoming, "session/new", json!({"cwd": cwd, "mcpServers": servers}), &t.cancel, |_| {}).await?;
                r.get("sessionId").and_then(|s| s.as_str()).ok_or_else(|| AiError::Provider("Kiro did not return a session id".into()))?.to_string()
            }
        };
        if let Some(save) = &t.save_session {
            save(&kiro_session);
        }

        let text = Arc::new(Mutex::new(String::new()));
        let foreign: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
        // Text after a tool call starts a new paragraph.
        let need_break = std::sync::atomic::AtomicBool::new(false);
        let (sid, rid, sink) = (t.session_id.clone(), t.run_id.clone(), t.sink.clone());
        let on_update = |u: &Value| {
            match u.get("sessionUpdate").and_then(|k| k.as_str()) {
                Some("agent_message_chunk") => {
                    if let Some(s) = u.pointer("/content/text").and_then(|s| s.as_str()) {
                        let mut t = text.lock();
                        let mut chunk = s.to_string();
                        if need_break.swap(false, Ordering::Relaxed) && !t.is_empty() && !t.ends_with('\n') {
                            chunk.insert_str(0, "\n\n");
                        }
                        t.push_str(&chunk);
                        sink.emit(AgentEvent::TextDelta { session_id: sid.clone(), run_id: rid.clone(), text: chunk });
                    }
                }
                Some("tool_call") | Some("tool_call_chunk") if is_databrain_tool(u) || u.get("sessionUpdate").and_then(|k| k.as_str()) == Some("tool_call_chunk") => {
                    need_break.store(true, Ordering::Relaxed);
                }
                Some("tool_call") => {
                    need_break.store(true, Ordering::Relaxed);
                    // DataBrain tools report through the MCP handler; show anything else Kiro does.
                    let id = u.get("toolCallId").and_then(|s| s.as_str()).unwrap_or_default().to_string();
                    let title = u.get("title").and_then(|s| s.as_str()).unwrap_or("Kiro tool").to_string();
                    foreign.lock().insert(id.clone(), title.clone());
                    sink.emit(AgentEvent::ToolStarted { session_id: sid.clone(), run_id: rid.clone(), call_id: id, tool: title, args: u.get("rawInput").cloned().unwrap_or_default() });
                }
                Some("tool_call_update") => {
                    let id = u.get("toolCallId").and_then(|s| s.as_str()).unwrap_or_default();
                    let done = matches!(u.get("status").and_then(|s| s.as_str()), Some("completed" | "failed"));
                    let title = if done { foreign.lock().remove(id) } else { None };
                    if let Some(title) = title {
                        let content = u.get("rawOutput").map(|v| trunc(&v.to_string(), 4000)).unwrap_or_default();
                        sink.emit(AgentEvent::ToolFinished { session_id: sid.clone(), run_id: rid.clone(), call_id: id.to_string(), tool: title, content, display: Value::Null });
                    }
                }
                _ => {}
            }
        };
        let prompt = json!({"sessionId": kiro_session, "prompt": [{"type": "text", "text": t.prompt}]});
        let result = acp.call(&mut incoming, "session/prompt", prompt, &t.cancel, on_update).await;
        if matches!(result, Err(AiError::Cancelled)) {
            // Ask Kiro to stop, give it a moment, then the child is killed on drop.
            let _ = acp.notify("session/cancel", json!({"sessionId": kiro_session})).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        drop(mcp);
        let r = result?;
        let final_text = text.lock().clone();
        match r.get("stopReason").and_then(|s| s.as_str()) {
            Some("refusal") => Err(AiError::Provider("Kiro declined the request".into())),
            Some("cancelled") => Err(AiError::Cancelled),
            Some("max_tokens") if final_text.is_empty() => Err(AiError::Provider("Kiro hit its output limit".into())),
            _ => Ok(final_text),
        }
    }
}

/// Persists the Kiro session id for a DataBrain conversation.
pub(crate) type SaveSession = Box<dyn Fn(&str) + Send + Sync>;

pub(crate) struct KiroTurn {
    pub session_id: String,
    pub run_id: String,
    pub prompt: String,
    pub model: Option<String>,
    pub ctx: Arc<ToolContext>,
    pub sink: Arc<dyn AgentSink>,
    pub cancel: CancellationToken,
    pub previous_session: Option<String>,
    pub save_session: Option<SaveSession>,
}

#[async_trait]
impl LlmProvider for KiroProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Kiro
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let (acp, mut incoming) = self.spawn_acp(None).await?;
        let cancel = CancellationToken::new();
        let init = json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}, "clientInfo": {"name": "databrain", "version": env!("CARGO_PKG_VERSION")}});
        acp.call(&mut incoming, "initialize", init, &cancel, |_| {}).await?;
        let r = acp.call(&mut incoming, "session/new", json!({"cwd": acp.cwd.to_string_lossy(), "mcpServers": []}), &cancel, |_| {}).await?;
        let models = r
            .pointer("/models/availableModels")
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| Some(ModelInfo { id: m.get("modelId")?.as_str()?.to_string(), name: m.get("description").and_then(|d| d.as_str()).map(str::to_string) }))
                    .collect()
            })
            .unwrap_or_default();
        Ok(models)
    }

    async fn chat_stream(&self, _req: ChatRequest) -> Result<BoxStream<'static, Result<ChatEvent>>> {
        Err(AiError::Config("Kiro runs as an agent through kiro-cli; use the assistant panel".into()))
    }

    fn as_kiro(&self) -> Option<&KiroProvider> {
        Some(self)
    }
}

// ------------------------------------------------------------------ helpers

fn not_installed() -> AiError {
    AiError::Config("kiro-cli not found. Install it from https://kiro.dev/cli (or set its path in the provider settings)".into())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

fn find_cli() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "kiro-cli.exe" } else { "kiro-cli" };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(h) = home() {
        dirs.push(h.join(".local").join("bin"));
        dirs.push(h.join(".kiro").join("bin"));
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(la).join("Programs").join("kiro-cli"));
    }
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file())
}

/// `(signed_in, account_type, identity)` from `whoami --format json`.
pub fn parse_whoami(out: &str) -> (bool, Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(out.trim()) else { return (false, None, None) };
    if v.get("account").is_some_and(|a| a.is_null()) {
        return (false, None, None);
    }
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let account_type = s("accountType").or_else(|| s("authMethod"));
    let identity = s("email").or_else(|| s("username")).or_else(|| s("startUrl"));
    (account_type.is_some() || identity.is_some(), account_type, identity)
}

fn is_databrain_tool(u: &Value) -> bool {
    u.pointer("/_meta/kiro/mcpServerName").and_then(|s| s.as_str()) == Some(MCP_NAME)
        || u.get("title").and_then(|s| s.as_str()).is_some_and(|t| t.contains(&format!("@{MCP_NAME}/")))
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated)", &s[..end])
}

fn open_login_terminal(program: &Path) -> Result<()> {
    let prog = program.to_string_lossy().to_string();
    let spawn = |c: &mut std::process::Command| c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ());
    #[cfg(target_os = "macos")]
    {
        let script = std::env::temp_dir().join("databrain-kiro-login.command");
        let body = format!(
            "#!/bin/sh\n# Opened by DataBrain to sign in to Kiro.\nunset KIRO_API_KEY\n'{}' login\necho\necho 'Done. You can close this window and return to DataBrain.'\n",
            prog.replace('\'', "'\\''")
        );
        std::fs::write(&script, body).map_err(|e| AiError::Internal(e.to_string()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700));
        }
        return spawn(std::process::Command::new("open").args(["-a", "Terminal"]).arg(&script)).map_err(|e| AiError::Internal(format!("cannot open Terminal: {e}")));
    }
    #[cfg(target_os = "windows")]
    {
        return spawn(std::process::Command::new("cmd").args(["/C", "start", "Kiro sign-in", "cmd", "/K", "set KIRO_API_KEY=&&"]).arg(&prog).arg("login"))
            .map_err(|e| AiError::Internal(format!("cannot open a terminal: {e}")));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let sh = format!("unset KIRO_API_KEY; '{}' login; echo; echo 'Done. You can close this window.'; read _", prog.replace('\'', "'\\''"));
        for (term, flag) in [("x-terminal-emulator", "-e"), ("gnome-terminal", "--"), ("konsole", "-e"), ("xfce4-terminal", "-x"), ("xterm", "-e")] {
            if spawn(std::process::Command::new(term).args([flag, "sh", "-c", &sh])).is_ok() {
                return Ok(());
            }
        }
        return Err(AiError::Config("no terminal emulator found; run `kiro-cli login` in a terminal, then retry".into()));
    }
    #[allow(unreachable_code)]
    Err(AiError::Config("run `kiro-cli login` in a terminal, then retry".into()))
}

// ------------------------------------------------------------------ ACP client

struct Acp {
    _child: Child,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    next: AtomicU64,
    stderr: Arc<Mutex<String>>,
    cwd: PathBuf,
}

impl Acp {
    async fn spawn(mut cmd: Command, cwd: PathBuf) -> Result<(Self, mpsc::UnboundedReceiver<Value>)> {
        let mut child = cmd.spawn().map_err(|e| AiError::Provider(format!("cannot start kiro-cli: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| AiError::Internal("no stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| AiError::Internal("no stdout".into()))?;
        let stderr_pipe = child.stderr.take();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> = Arc::default();
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut e) = stderr_pipe {
            let buf = stderr.clone();
            tokio::spawn(async move {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = e.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let mut b = buf.lock();
                    b.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if b.len() > 8000 {
                        let cut = b.len() - 4000;
                        let cut = (cut..b.len()).find(|i| b.is_char_boundary(*i)).unwrap_or(b.len());
                        b.drain(..cut);
                    }
                }
            });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let (p, errbuf) = (pending.clone(), stderr.clone());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                let is_response = msg.get("method").is_none() && (msg.get("result").is_some() || msg.get("error").is_some());
                if is_response {
                    let waiter = msg.get("id").and_then(|i| i.as_u64()).and_then(|id| p.lock().remove(&id));
                    if let Some(waiter) = waiter {
                        let r = match msg.get("error") {
                            Some(e) => Err(AiError::Provider(format!("Kiro: {}", e.get("message").and_then(|m| m.as_str()).unwrap_or("request failed")))),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = waiter.send(r);
                    }
                } else if tx.send(msg).is_err() {
                    break;
                }
            }
            // Process ended: fail everything still waiting.
            let tail = errbuf.lock().trim().to_string();
            let why = if tail.is_empty() { "kiro-cli exited".to_string() } else { format!("kiro-cli exited: {}", trunc(&tail, 600)) };
            for (_, w) in p.lock().drain() {
                let _ = w.send(Err(AiError::Provider(why.clone())));
            }
        });
        Ok((Self { _child: child, stdin: tokio::sync::Mutex::new(stdin), pending, next: AtomicU64::new(1), stderr, cwd }, rx))
    }

    async fn write(&self, v: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(v).map_err(|e| AiError::Internal(e.to_string()))?;
        line.push(b'\n');
        let mut w = self.stdin.lock().await;
        w.write_all(&line).await.map_err(|e| AiError::Provider(format!("kiro-cli: {e}")))?;
        w.flush().await.map_err(|e| AiError::Provider(format!("kiro-cli: {e}")))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    /// Send a request and pump agent→client messages until its response.
    async fn call(
        &self,
        incoming: &mut mpsc::UnboundedReceiver<Value>,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
        mut on_update: impl FnMut(&Value),
    ) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let timeout = if method == "session/prompt" { Duration::from_secs(1800) } else { Duration::from_secs(90) };
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                r = &mut rx => {
                    // The reader queues notifications before it resolves the
                    // response that follows them: flush those first so the
                    // last message chunks are not lost.
                    while let Ok(msg) = incoming.try_recv() {
                        self.handle_incoming(&msg, &mut on_update).await?;
                    }
                    return r.unwrap_or_else(|_| Err(AiError::Provider("kiro-cli exited".into())));
                }
                Some(msg) = incoming.recv() => self.handle_incoming(&msg, &mut on_update).await?,
                _ = cancel.cancelled() => {
                    self.pending.lock().remove(&id);
                    return Err(AiError::Cancelled);
                }
                _ = &mut deadline => {
                    self.pending.lock().remove(&id);
                    let tail = self.stderr.lock().trim().to_string();
                    return Err(AiError::Provider(format!("Kiro did not answer {method} in time{}", if tail.is_empty() { String::new() } else { format!(": {}", trunc(&tail, 300)) })));
                }
            }
        }
    }

    async fn handle_incoming(&self, msg: &Value, on_update: &mut impl FnMut(&Value)) -> Result<()> {
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default();
        let id = msg.get("id").cloned();
        match (method, id) {
            ("session/update" | "_kiro.dev/session/update", None) => {
                if let Some(u) = msg.pointer("/params/update") {
                    on_update(u);
                }
            }
            ("session/request_permission", Some(id)) => {
                let params = msg.get("params").cloned().unwrap_or_default();
                let tool = params.get("toolCall").cloned().unwrap_or_default();
                let allow = is_databrain_tool(&tool);
                let options = params.get("options").and_then(|o| o.as_array()).cloned().unwrap_or_default();
                let pick = |prefix: &str| {
                    options.iter().find(|o| o.get("kind").and_then(|k| k.as_str()).is_some_and(|k| k.starts_with(prefix))).and_then(|o| o.get("optionId").cloned())
                };
                // DataBrain tools enforce their own approvals; anything else is refused.
                let outcome = match pick(if allow { "allow" } else { "reject" }) {
                    Some(opt) => json!({"outcome": "selected", "optionId": opt}),
                    None => json!({"outcome": "cancelled"}),
                };
                self.write(&json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}})).await?;
            }
            (_, Some(id)) => {
                // fs/*, terminal/* … are not offered by DataBrain.
                self.write(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported by DataBrain"}})).await?;
            }
            _ => {}
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ MCP over HTTP (per turn)

/// Minimal MCP "streamable HTTP" server for one Kiro turn. JSON responses
/// only (no server-initiated stream). Stops when dropped.
pub struct McpHttp {
    url: String,
    token: String,
    stop: CancellationToken,
}

impl Drop for McpHttp {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl McpHttp {
    pub async fn start(ctx: Arc<ToolContext>, sink: Arc<dyn AgentSink>, session_id: String, run_id: String) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| AiError::Internal(format!("cannot open local tool endpoint: {e}")))?;
        let port = listener.local_addr().map_err(|e| AiError::Internal(e.to_string()))?.port();
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let stop = CancellationToken::new();
        let shared = Arc::new(McpShared { ctx, sink, session_id, run_id, token: token.clone(), port });
        let s2 = stop.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = tokio::select! {
                    a = listener.accept() => match a { Ok(a) => a, Err(_) => continue },
                    _ = s2.cancelled() => break,
                };
                let (sh, stop) = (shared.clone(), s2.clone());
                tokio::spawn(async move {
                    tokio::select! {
                        _ = serve_conn(sock, sh) => {}
                        _ = stop.cancelled() => {}
                    }
                });
            }
        });
        Ok(Self { url: format!("http://127.0.0.1:{port}/mcp"), token, stop })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn token(&self) -> &str {
        &self.token
    }

    fn server_entry(&self) -> Value {
        json!({"type": "http", "name": MCP_NAME, "url": self.url, "headers": [{"name": "Authorization", "value": format!("Bearer {}", self.token)}]})
    }
}

struct McpShared {
    ctx: Arc<ToolContext>,
    sink: Arc<dyn AgentSink>,
    session_id: String,
    run_id: String,
    token: String,
    port: u16,
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn serve_conn(mut sock: tokio::net::TcpStream, sh: Arc<McpShared>) {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    loop {
        // Read the request head.
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if buf.len() > 64 * 1024 {
                return;
            }
            let mut chunk = [0u8; 8192];
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let headers: HashMap<String, String> =
            lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
        let len: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
        if len > 4 * 1024 * 1024 {
            let _ = sock.write_all(&http_response("413 Payload Too Large", None, b"")).await;
            return;
        }
        while buf.len() < head_end + len {
            let mut chunk = [0u8; 8192];
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let body = buf[head_end..head_end + len].to_vec();
        buf.drain(..head_end + len);

        let mut parts = request_line.split(' ');
        let (method, path) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let host_ok = headers.get("host").is_some_and(|h| *h == format!("127.0.0.1:{}", sh.port) || *h == format!("localhost:{}", sh.port));
        let auth_ok = headers.get("authorization").and_then(|a| a.strip_prefix("Bearer ")).is_some_and(|t| ct_eq(t.as_bytes(), sh.token.as_bytes()));
        let resp = if path.split('?').next() != Some("/mcp") {
            http_response("404 Not Found", None, b"")
        } else if headers.contains_key("origin") || !host_ok {
            // Browsers send Origin; refuse web pages (DNS rebinding / CSRF).
            http_response("403 Forbidden", None, b"")
        } else if !auth_ok {
            http_response("401 Unauthorized", None, b"")
        } else {
            match method {
                "POST" => match serde_json::from_slice::<Value>(&body) {
                    Ok(Value::Array(batch)) => {
                        let mut out = Vec::new();
                        for m in batch {
                            if let Some(r) = handle_mcp(&sh, &m).await {
                                out.push(r);
                            }
                        }
                        if out.is_empty() { http_response("202 Accepted", None, b"") } else { http_response("200 OK", Some("application/json"), &serde_json::to_vec(&out).unwrap_or_default()) }
                    }
                    Ok(m) => match handle_mcp(&sh, &m).await {
                        Some(r) => http_response("200 OK", Some("application/json"), &serde_json::to_vec(&r).unwrap_or_default()),
                        None => http_response("202 Accepted", None, b""),
                    },
                    Err(_) => http_response("400 Bad Request", None, b""),
                },
                "DELETE" => http_response("200 OK", None, b""),
                _ => http_response("405 Method Not Allowed", None, b""),
            }
        };
        if sock.write_all(&resp).await.is_err() {
            return;
        }
        if headers.get("connection").is_some_and(|c| c.eq_ignore_ascii_case("close")) {
            return;
        }
    }
}

fn http_response(status: &str, ctype: Option<&str>, body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n", body.len());
    if let Some(c) = ctype {
        out.push_str(&format!("Content-Type: {c}\r\n"));
    }
    out.push_str("\r\n");
    let mut v = out.into_bytes();
    v.extend_from_slice(body);
    v
}

async fn handle_mcp(sh: &McpShared, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?; // notifications need no answer
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or_default();
    let ok = |result: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    match method {
        "initialize" => ok(json!({
            "protocolVersion": params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("2025-03-26"),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "databrain", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "DataBrain database tools for the current connection. Tools that run SQL or write to the editor ask the user in DataBrain."
        })),
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({"tools": specs(true).into_iter().map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.parameters})).collect::<Vec<_>>()})),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
            let args = params.get("arguments").cloned().filter(|a| a.is_object()).unwrap_or_else(|| json!({}));
            let call_id = uuid::Uuid::new_v4().to_string();
            sh.sink.emit(AgentEvent::ToolStarted { session_id: sh.session_id.clone(), run_id: sh.run_id.clone(), call_id: call_id.clone(), tool: name.clone(), args: args.clone() });
            let o = sh.ctx.call(&name, &args).await;
            let content = trunc(&o.content, MAX_TOOL_CONTENT);
            sh.sink.emit(AgentEvent::ToolFinished { session_id: sh.session_id.clone(), run_id: sh.run_id.clone(), call_id, tool: name, content: content.clone(), display: o.display });
            let is_error = content.starts_with("ERROR");
            ok(json!({"content": [{"type": "text", "text": content}], "isError": is_error}))
        }
        _ => Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {method}")}})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whoami() {
        assert_eq!(parse_whoami(r#"{"account":null}"#), (false, None, None));
        assert_eq!(parse_whoami(r#"{"accountType":"ApiKey","email":"a@b.c"}"#), (true, Some("ApiKey".into()), Some("a@b.c".into())));
        assert!(!parse_whoami("not json").0);
    }

    #[test]
    fn detects_databrain_tools() {
        assert!(is_databrain_tool(&json!({"title": "Running: @databrain/run_query"})));
        assert!(is_databrain_tool(&json!({"_meta": {"kiro": {"mcpServerName": "databrain"}}})));
        assert!(!is_databrain_tool(&json!({"title": "Running: shell"})));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"ab", b"abc"));
        assert_eq!(trunc("héllo", 2), "h…(truncated)");
    }
}
