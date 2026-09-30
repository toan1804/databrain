//! LLM provider adapters. One OpenAI-compatible adapter covers OpenAI,
//! Azure OpenAI, OpenRouter, Ollama, LM Studio, vLLM and llama.cpp; Anthropic
//! and Gemini have dedicated adapters.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use databrain_auth::{CredentialSource, ExposeSecret, SecretRef, SecretStore};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::sse;
use crate::types::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Openai,
    Anthropic,
    Gemini,
    AzureOpenai,
    Openrouter,
    Ollama,
    LmStudio,
    /// Any other OpenAI-compatible endpoint (vLLM, llama.cpp, gateways).
    OpenaiCompatible,
    /// Kiro, driven through the local `kiro-cli` (Agent Client Protocol).
    Kiro,
}

impl ProviderKind {
    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(Value::String(s.to_string())).ok()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Openai => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
            ProviderKind::AzureOpenai => "azure_openai",
            ProviderKind::Openrouter => "openrouter",
            ProviderKind::Ollama => "ollama",
            ProviderKind::LmStudio => "lm_studio",
            ProviderKind::OpenaiCompatible => "openai_compatible",
            ProviderKind::Kiro => "kiro",
        }
    }
    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderKind::Openai => "https://api.openai.com/v1",
            ProviderKind::Anthropic => "https://api.anthropic.com/v1",
            ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            ProviderKind::AzureOpenai => "",
            ProviderKind::Openrouter => "https://openrouter.ai/api/v1",
            ProviderKind::Ollama => "http://localhost:11434/v1",
            ProviderKind::LmStudio => "http://localhost:1234/v1",
            ProviderKind::OpenaiCompatible => "",
            ProviderKind::Kiro => "",
        }
    }
    /// Local runtimes don't need a key.
    pub fn is_local(self) -> bool {
        matches!(self, ProviderKind::Ollama | ProviderKind::LmStudio)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuth {
    #[default]
    ApiKey,
    /// OpenRouter browser sign-in (PKCE) — yields a user API key.
    BrowserOpenrouter,
    /// Microsoft Entra ID token (Azure OpenAI) via `az login`.
    AzureCli,
    /// Google ADC bearer token (Vertex-style gateways / Gemini with OAuth).
    GoogleAdc,
    /// Kiro browser sign-in (`kiro-cli login` session: Builder ID, GitHub,
    /// Google, IAM Identity Center).
    KiroBrowser,
    None,
}

/// Non-secret provider settings stored in `AiProviderRecord.config`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub auth: ProviderAuth,
    pub default_model: Option<String>,
    /// Smaller/faster model for inline edits and titles.
    pub fast_model: Option<String>,
    /// Azure deployment API version.
    pub api_version: Option<String>,
    pub extra_headers: BTreeMap<String, String>,
    pub max_output_tokens: Option<u32>,
    /// CLI providers (Kiro): extra environment for the child process.
    pub cli_env: BTreeMap<String, String>,
    /// Kiro: directory for the managed agent config (default ~/.kiro/agents).
    pub agents_dir: Option<String>,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn chat_stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatEvent>>>;
    /// Agent-style providers (Kiro) run whole turns instead of chat calls.
    fn as_kiro(&self) -> Option<&crate::kiro::KiroProvider> {
        None
    }
}

/// Resolves the API key / bearer token for a provider on each request.
#[derive(Clone)]
pub enum KeySource {
    None,
    Stored { store: Arc<dyn SecretStore>, reference: SecretRef },
    Credential(Arc<dyn CredentialSource>),
    Inline(String),
}

impl KeySource {
    pub(crate) async fn get(&self) -> Result<Option<String>> {
        Ok(match self {
            KeySource::None => None,
            KeySource::Inline(k) => Some(k.clone()),
            KeySource::Stored { store, reference } => store.get(reference)?.map(|s| s.expose_secret().to_string()).filter(|s| !s.is_empty()),
            KeySource::Credential(c) => match c.get().await? {
                databrain_auth::Credential::Bearer { token, .. } => Some(token.expose_secret().to_string()),
                _ => None,
            },
        })
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .user_agent(concat!("DataBrain/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

fn net(e: reqwest::Error) -> AiError {
    AiError::Network(e.to_string())
}

async fn check(resp: reqwest::Response, provider: &str) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let msg = v
        .pointer("/error/message")
        .or(v.get("message"))
        .or(v.get("error"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(400).collect());
    let err = format!("{provider} ({status}): {msg}");
    Err(if status.as_u16() == 401 || status.as_u16() == 403 { AiError::Auth(err) } else { AiError::Provider(err) })
}

/// Build a provider from its record + key source.
pub fn build(kind: ProviderKind, cfg: &ProviderConfig, key: KeySource) -> Result<Arc<dyn LlmProvider>> {
    if kind == ProviderKind::Kiro {
        return Ok(Arc::new(crate::kiro::KiroProvider::new(cfg, key)?));
    }
    let base = cfg
        .base_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| kind.default_base_url().to_string())
        .trim_end_matches('/')
        .to_string();
    if base.is_empty() {
        return Err(AiError::Config("this provider needs a base URL".into()));
    }
    Ok(match kind {
        ProviderKind::Anthropic => Arc::new(Anthropic { base, key, http: http(), max_tokens: cfg.max_output_tokens.unwrap_or(8192) }),
        ProviderKind::Gemini => Arc::new(Gemini { base, key, http: http(), bearer: cfg.auth == ProviderAuth::GoogleAdc }),
        _ => Arc::new(OpenAi {
            kind,
            base,
            key,
            http: http(),
            azure_api_version: (kind == ProviderKind::AzureOpenai)
                .then(|| cfg.api_version.clone().unwrap_or_else(|| "2024-10-21".into())),
            azure_bearer: cfg.auth == ProviderAuth::AzureCli,
            headers: cfg.extra_headers.clone(),
        }),
    })
}

// ------------------------------------------------------------------ OpenAI-compatible

pub struct OpenAi {
    kind: ProviderKind,
    base: String,
    key: KeySource,
    http: reqwest::Client,
    azure_api_version: Option<String>,
    azure_bearer: bool,
    headers: BTreeMap<String, String>,
}

impl OpenAi {
    async fn req(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let url = match &self.azure_api_version {
            Some(v) => format!("{}{path}?api-version={v}", self.base),
            None => format!("{}{path}", self.base),
        };
        let mut r = self.http.request(method, url);
        if let Some(k) = self.key.get().await? {
            r = if self.azure_api_version.is_some() && !self.azure_bearer { r.header("api-key", k) } else { r.bearer_auth(k) };
        }
        if self.kind == ProviderKind::Openrouter {
            r = r.header("HTTP-Referer", "https://databrain.dev").header("X-Title", "DataBrain");
        }
        for (k, v) in &self.headers {
            r = r.header(k, v);
        }
        Ok(r)
    }
}

pub fn openai_messages(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];
    for m in messages {
        match m.role {
            Role::User => out.push(json!({"role": "user", "content": m.text})),
            Role::Assistant => {
                let mut v = json!({"role": "assistant", "content": if m.text.is_empty() { Value::Null } else { Value::String(m.text.clone()) }});
                if !m.tool_calls.is_empty() {
                    v["tool_calls"] = m
                        .tool_calls
                        .iter()
                        .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments.to_string()}}))
                        .collect();
                }
                out.push(v);
            }
            Role::Tool => out.push(json!({"role": "tool", "tool_call_id": m.tool_call_id, "content": m.text})),
        }
    }
    out
}

#[async_trait]
impl LlmProvider for OpenAi {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if self.kind == ProviderKind::AzureOpenai {
            // Azure lists deployments differently; the deployment is the model.
            return Ok(vec![]);
        }
        let resp = check(self.req(reqwest::Method::GET, "/models").await?.send().await.map_err(net)?, "models").await?;
        let v: Value = resp.json().await.map_err(net)?;
        let mut out: Vec<ModelInfo> = v["data"]
            .as_array()
            .or(v["models"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        Some(ModelInfo { id: m["id"].as_str().or(m["name"].as_str())?.to_string(), name: m["name"].as_str().map(str::to_string) })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatEvent>>> {
        let path = if self.azure_api_version.is_some() { format!("/openai/deployments/{}/chat/completions", req.model) } else { "/chat/completions".into() };
        let mut body = json!({
            "model": req.model,
            "messages": openai_messages(&req.system, &req.messages),
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
                .collect();
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = json!(m);
        }
        let resp = self.req(reqwest::Method::POST, &path).await?.json(&body).send().await.map_err(net)?;
        let resp = check(resp, self.kind.as_str()).await?;
        Ok(openai_stream(resp.bytes_stream().boxed()))
    }
}

/// Parse an OpenAI chat-completions SSE stream (tool-call argument deltas
/// are assembled by index).
pub fn openai_stream(bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>) -> BoxStream<'static, Result<ChatEvent>> {
    let events = sse::events(bytes);
    let state: (BTreeMap<u64, (String, String, String)>, bool) = (BTreeMap::new(), false);
    futures::stream::unfold((Box::pin(events), state), |(mut ev, (mut calls, mut finished))| async move {
        loop {
            if finished {
                return None;
            }
            let Some(e) = ev.next().await else {
                // Stream ended: flush tool calls.
                finished = true;
                if !calls.is_empty() {
                    let out: Vec<Result<ChatEvent>> = std::mem::take(&mut calls).into_values().map(|(id, name, args)| Ok(ChatEvent::ToolCall(parse_call(id, name, &args)))).collect();
                    return Some((futures::stream::iter(out).boxed(), (ev, (calls, finished))));
                }
                return Some((futures::stream::iter(vec![Ok(ChatEvent::Done { stop_reason: "end".into() })]).boxed(), (ev, (calls, finished))));
            };
            let e = match e {
                Ok(e) => e,
                Err(err) => return Some((futures::stream::iter(vec![Err(AiError::Network(err))]).boxed(), (ev, (calls, true)))),
            };
            if e.data.trim() == "[DONE]" {
                finished = true;
                let mut out: Vec<Result<ChatEvent>> = std::mem::take(&mut calls).into_values().map(|(id, name, args)| Ok(ChatEvent::ToolCall(parse_call(id, name, &args)))).collect();
                out.push(Ok(ChatEvent::Done { stop_reason: "stop".into() }));
                return Some((futures::stream::iter(out).boxed(), (ev, (calls, finished))));
            }
            let Ok(v) = serde_json::from_str::<Value>(&e.data) else { continue };
            if let Some(err) = v.pointer("/error/message").and_then(|m| m.as_str()) {
                return Some((futures::stream::iter(vec![Err(AiError::Provider(err.to_string()))]).boxed(), (ev, (calls, true))));
            }
            let mut out: Vec<Result<ChatEvent>> = Vec::new();
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                out.push(Ok(ChatEvent::Usage {
                    input: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                    output: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
                }));
            }
            if let Some(choice) = v["choices"].get(0) {
                let d = &choice["delta"];
                if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
                    out.push(Ok(ChatEvent::TextDelta(t.to_string())));
                }
                for tc in d["tool_calls"].as_array().cloned().unwrap_or_default() {
                    let idx = tc["index"].as_u64().unwrap_or(0);
                    let entry = calls.entry(idx).or_insert_with(|| (String::new(), String::new(), String::new()));
                    if let Some(id) = tc["id"].as_str() {
                        entry.0 = id.to_string();
                    }
                    if let Some(n) = tc.pointer("/function/name").and_then(|n| n.as_str()) {
                        entry.1.push_str(n);
                    }
                    if let Some(a) = tc.pointer("/function/arguments").and_then(|n| n.as_str()) {
                        entry.2.push_str(a);
                    }
                }
                if choice["finish_reason"].is_string() && !calls.is_empty() {
                    out.extend(std::mem::take(&mut calls).into_values().map(|(id, name, args)| Ok(ChatEvent::ToolCall(parse_call(id, name, &args)))));
                }
            }
            if !out.is_empty() {
                return Some((futures::stream::iter(out).boxed(), (ev, (calls, finished))));
            }
        }
    })
    .flatten()
    .boxed()
}

fn parse_call(id: String, name: String, args: &str) -> ToolCall {
    let arguments = if args.trim().is_empty() { json!({}) } else { serde_json::from_str(args).unwrap_or_else(|_| json!({"_raw": args})) };
    ToolCall { id: if id.is_empty() { uuid::Uuid::new_v4().to_string() } else { id }, name, arguments }
}

// ------------------------------------------------------------------ Anthropic

pub struct Anthropic {
    base: String,
    key: KeySource,
    http: reqwest::Client,
    max_tokens: u32,
}

pub fn anthropic_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let (role, block) = match m.role {
            Role::User => ("user", vec![json!({"type": "text", "text": m.text})]),
            Role::Assistant => {
                let mut b = Vec::new();
                if !m.text.is_empty() {
                    b.push(json!({"type": "text", "text": m.text}));
                }
                for c in &m.tool_calls {
                    b.push(json!({"type": "tool_use", "id": c.id, "name": c.name, "input": c.arguments}));
                }
                ("assistant", b)
            }
            Role::Tool => ("user", vec![json!({"type": "tool_result", "tool_use_id": m.tool_call_id, "content": m.text})]),
        };
        // Merge consecutive same-role messages (tool results follow each other).
        match out.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(arr) = last["content"].as_array_mut() {
                    arr.extend(block);
                }
            }
            _ => out.push(json!({"role": role, "content": block})),
        }
    }
    out
}

#[async_trait]
impl LlmProvider for Anthropic {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let key = self.key.get().await?.ok_or_else(|| AiError::Auth("API key is not set".into()))?;
        let resp = self
            .http
            .get(format!("{}/models?limit=100", self.base))
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(net)?;
        let v: Value = check(resp, "Anthropic").await?.json().await.map_err(net)?;
        Ok(v["data"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| Some(ModelInfo { id: m["id"].as_str()?.into(), name: m["display_name"].as_str().map(str::to_string) })).collect())
            .unwrap_or_default())
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatEvent>>> {
        let key = self.key.get().await?.ok_or_else(|| AiError::Auth("API key is not set".into()))?;
        let mut body = json!({
            "model": req.model,
            "system": req.system,
            "messages": anthropic_messages(&req.messages),
            "max_tokens": req.max_tokens.unwrap_or(self.max_tokens),
            "stream": true,
        });
        if !req.tools.is_empty() {
            body["tools"] = req.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters})).collect();
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        let resp = self
            .http
            .post(format!("{}/messages", self.base))
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(net)?;
        let resp = check(resp, "Anthropic").await?;
        Ok(anthropic_stream(resp.bytes_stream().boxed()))
    }
}

pub fn anthropic_stream(bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>) -> BoxStream<'static, Result<ChatEvent>> {
    let events = sse::events(bytes);
    // block index -> (id, name, partial json)
    let blocks: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    futures::stream::unfold((Box::pin(events), blocks, 0u32), |(mut ev, mut blocks, mut input_tokens)| async move {
        loop {
            let e = match ev.next().await? {
                Ok(e) => e,
                Err(err) => return Some((vec![Err(AiError::Network(err))], (ev, blocks, input_tokens))),
            };
            let Ok(v) = serde_json::from_str::<Value>(&e.data) else { continue };
            let mut out: Vec<Result<ChatEvent>> = Vec::new();
            match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    input_tokens = v.pointer("/message/usage/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0) as u32;
                }
                "content_block_start" => {
                    let idx = v["index"].as_u64().unwrap_or(0);
                    let b = &v["content_block"];
                    if b["type"] == "tool_use" {
                        blocks.insert(idx, (b["id"].as_str().unwrap_or("").into(), b["name"].as_str().unwrap_or("").into(), String::new()));
                    }
                }
                "content_block_delta" => {
                    let d = &v["delta"];
                    match d["type"].as_str() {
                        Some("text_delta") => out.push(Ok(ChatEvent::TextDelta(d["text"].as_str().unwrap_or("").into()))),
                        Some("input_json_delta") => {
                            if let Some(b) = blocks.get_mut(&v["index"].as_u64().unwrap_or(0)) {
                                b.2.push_str(d["partial_json"].as_str().unwrap_or(""));
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    if let Some((id, name, args)) = blocks.remove(&v["index"].as_u64().unwrap_or(0)) {
                        out.push(Ok(ChatEvent::ToolCall(parse_call(id, name, &args))));
                    }
                }
                "message_delta" => {
                    if let Some(o) = v.pointer("/usage/output_tokens").and_then(|t| t.as_u64()) {
                        out.push(Ok(ChatEvent::Usage { input: input_tokens, output: o as u32 }));
                    }
                }
                "message_stop" => out.push(Ok(ChatEvent::Done { stop_reason: "stop".into() })),
                "error" => out.push(Err(AiError::Provider(v.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("error").into()))),
                _ => {}
            }
            if !out.is_empty() {
                return Some((out, (ev, blocks, input_tokens)));
            }
        }
    })
    .flat_map(futures::stream::iter)
    .boxed()
}

// ------------------------------------------------------------------ Gemini

pub struct Gemini {
    base: String,
    key: KeySource,
    http: reqwest::Client,
    bearer: bool,
}

/// Gemini accepts a subset of JSON Schema; strip unsupported keywords.
fn gemini_schema(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| !matches!(k.as_str(), "additionalProperties" | "$schema" | "default" | "examples"))
                .map(|(k, v)| (k.clone(), gemini_schema(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(gemini_schema).collect()),
        other => other.clone(),
    }
}

pub fn gemini_contents(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let (role, parts) = match m.role {
            Role::User => ("user", vec![json!({"text": m.text})]),
            Role::Assistant => {
                let mut p = Vec::new();
                if !m.text.is_empty() {
                    p.push(json!({"text": m.text}));
                }
                for c in &m.tool_calls {
                    p.push(json!({"functionCall": {"name": c.name, "args": c.arguments}}));
                }
                ("model", p)
            }
            Role::Tool => ("user", vec![json!({"functionResponse": {"name": m.tool_name, "response": {"result": m.text}}})]),
        };
        match out.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(arr) = last["parts"].as_array_mut() {
                    arr.extend(parts);
                }
            }
            _ => out.push(json!({"role": role, "parts": parts})),
        }
    }
    out
}

impl Gemini {
    async fn auth(&self, r: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let key = self.key.get().await?.ok_or_else(|| AiError::Auth("API key is not set".into()))?;
        Ok(if self.bearer { r.bearer_auth(key) } else { r.header("x-goog-api-key", key) })
    }
}

#[async_trait]
impl LlmProvider for Gemini {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gemini
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let r = self.auth(self.http.get(format!("{}/models?pageSize=200", self.base))).await?;
        let v: Value = check(r.send().await.map_err(net)?, "Gemini").await?.json().await.map_err(net)?;
        Ok(v["models"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|m| m["supportedGenerationMethods"].as_array().is_some_and(|x| x.iter().any(|s| s == "generateContent")))
                    .filter_map(|m| Some(ModelInfo { id: m["name"].as_str()?.trim_start_matches("models/").into(), name: m["displayName"].as_str().map(str::to_string) }))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatEvent>>> {
        let mut body = json!({
            "systemInstruction": {"parts": [{"text": req.system}]},
            "contents": gemini_contents(&req.messages),
        });
        if !req.tools.is_empty() {
            body["tools"] = json!([{"functionDeclarations": req.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parameters": gemini_schema(&t.parameters)})).collect::<Vec<_>>()}]);
        }
        let mut gen_cfg = json!({});
        if let Some(t) = req.temperature {
            gen_cfg["temperature"] = json!(t);
        }
        if let Some(m) = req.max_tokens {
            gen_cfg["maxOutputTokens"] = json!(m);
        }
        body["generationConfig"] = gen_cfg;
        let r = self.auth(self.http.post(format!("{}/models/{}:streamGenerateContent?alt=sse", self.base, req.model))).await?;
        let resp = check(r.json(&body).send().await.map_err(net)?, "Gemini").await?;
        let events = sse::events(resp.bytes_stream().boxed());
        Ok(events
            .map(|e| -> Vec<Result<ChatEvent>> {
                let e = match e {
                    Ok(e) => e,
                    Err(err) => return vec![Err(AiError::Network(err))],
                };
                let Ok(v) = serde_json::from_str::<Value>(&e.data) else { return vec![] };
                let mut out = Vec::new();
                for p in v.pointer("/candidates/0/content/parts").and_then(|p| p.as_array()).cloned().unwrap_or_default() {
                    if let Some(t) = p["text"].as_str() {
                        out.push(Ok(ChatEvent::TextDelta(t.into())));
                    }
                    if let Some(fc) = p.get("functionCall") {
                        out.push(Ok(ChatEvent::ToolCall(ToolCall {
                            id: uuid::Uuid::new_v4().to_string(),
                            name: fc["name"].as_str().unwrap_or("").into(),
                            arguments: fc.get("args").cloned().unwrap_or(json!({})),
                        })));
                    }
                }
                if let Some(u) = v.get("usageMetadata") {
                    if v.pointer("/candidates/0/finishReason").is_some() {
                        out.push(Ok(ChatEvent::Usage {
                            input: u["promptTokenCount"].as_u64().unwrap_or(0) as u32,
                            output: u["candidatesTokenCount"].as_u64().unwrap_or(0) as u32,
                        }));
                    }
                }
                if let Some(r) = v.pointer("/candidates/0/finishReason").and_then(|r| r.as_str()) {
                    out.push(Ok(ChatEvent::Done { stop_reason: r.to_lowercase() }));
                }
                out
            })
            .flat_map(futures::stream::iter)
            .boxed())
    }
}

// ------------------------------------------------------------------ OpenRouter browser sign-in

/// OpenRouter PKCE sign-in: opens the browser, receives a code on a loopback
/// redirect and exchanges it for a user-controlled API key.
pub async fn openrouter_sign_in(interaction: &dyn databrain_auth::Interaction) -> Result<String> {
    // OpenRouter accepts any localhost callback; prefer the documented port.
    let lb = match databrain_auth::loopback::Loopback::bind("localhost", 3000, "/callback").await {
        Ok(lb) => lb,
        Err(_) => databrain_auth::loopback::Loopback::bind("localhost", 0, "/callback").await?,
    };
    let callback = lb.redirect_uri();
    let p = databrain_auth::oauth::pkce();
    let url = format!(
        "https://openrouter.ai/auth?callback_url={}&code_challenge={}&code_challenge_method=S256",
        urlencode(&callback),
        p.challenge
    );
    interaction.open_url(&url).await?;
    let params = lb.wait(None, &interaction.cancel_token(), Duration::from_secs(300)).await;
    interaction.finished().await;
    let code = params?.get("code").cloned().ok_or_else(|| AiError::Auth("OpenRouter did not return a code".into()))?;
    let resp = http()
        .post("https://openrouter.ai/api/v1/auth/keys")
        .json(&json!({"code": code, "code_verifier": p.verifier, "code_challenge_method": "S256"}))
        .send()
        .await
        .map_err(net)?;
    let v: Value = check(resp, "OpenRouter").await?.json().await.map_err(net)?;
    v["key"].as_str().map(str::to_string).ok_or_else(|| AiError::Auth("OpenRouter did not return a key".into()))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_stream(chunks: Vec<&'static str>) -> BoxStream<'static, reqwest::Result<bytes::Bytes>> {
        futures::stream::iter(chunks.into_iter().map(|c| Ok(bytes::Bytes::from(c)))).boxed()
    }

    #[tokio::test]
    async fn parses_openai_stream_with_tool_calls() {
        let s = openai_stream(bytes_stream(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"run_query\",\"arguments\":\"{\\\"sq\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"l\\\":\\\"select 1\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\ndata: [DONE]\n\n",
        ]));
        let evs: Vec<ChatEvent> = s.map(|e| e.unwrap()).collect().await;
        assert_eq!(evs[0], ChatEvent::TextDelta("Hel".into()));
        assert_eq!(
            evs[2],
            ChatEvent::ToolCall(ToolCall { id: "c1".into(), name: "run_query".into(), arguments: json!({"sql": "select 1"}) })
        );
        assert_eq!(evs[3], ChatEvent::Usage { input: 10, output: 5 });
        assert!(matches!(evs.last(), Some(ChatEvent::Done { .. })));
    }

    #[tokio::test]
    async fn parses_anthropic_stream() {
        let s = anthropic_stream(bytes_stream(vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"describe_table\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"table\\\": \\\"users\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
        ]));
        let evs: Vec<ChatEvent> = s.map(|e| e.unwrap()).collect().await;
        assert_eq!(evs[0], ChatEvent::TextDelta("Hi".into()));
        assert_eq!(evs[1], ChatEvent::ToolCall(ToolCall { id: "t1".into(), name: "describe_table".into(), arguments: json!({"table": "users"}) }));
        assert_eq!(evs[2], ChatEvent::Usage { input: 12, output: 7 });
    }

    #[test]
    fn message_translation() {
        let call = ToolCall { id: "c".into(), name: "f".into(), arguments: json!({"a": 1}) };
        let msgs = vec![Message::user("q"), Message::assistant("", vec![call.clone()]), Message::tool(&call, "r1"), Message::tool(&call, "r2")];
        let a = anthropic_messages(&msgs);
        assert_eq!(a.len(), 3);
        assert_eq!(a[2]["content"].as_array().unwrap().len(), 2);
        let o = openai_messages("sys", &msgs);
        assert_eq!(o[0]["role"], "system");
        assert_eq!(o[2]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        let g = gemini_contents(&msgs);
        assert_eq!(g[1]["role"], "model");
        assert_eq!(g[2]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(gemini_schema(&json!({"type":"object","additionalProperties":false})), json!({"type":"object"}));
    }
}
