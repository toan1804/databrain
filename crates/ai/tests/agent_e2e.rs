//! End-to-end agent test: a mock OpenAI-compatible server drives the agent
//! through search_schema → run_query → query_result → final answer against a
//! real SQLite database.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use databrain_ai::providers::ProviderConfig;
use databrain_ai::{Agent, AgentEvent, AgentRequest, AgentSink, Mode, ToolHost, UiContext};
use databrain_auth::{AuthMethod, MemoryStore, SecretRef, SecretStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind, ConnectorRegistry};
use databrain_query_engine::{EventHub, JobEvent, QueryEngine, RunRequest};
use databrain_result_store::ResultStore;
use databrain_workspace::{AiPolicy, AiProviderRecord, ConnectionProfile, EnvTag, HistoryQuery, Origin, RunQueryPolicy, Workspace};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct NullSink;
impl databrain_query_engine::EventSink for NullSink {
    fn emit(&self, _: JobEvent) {}
}

#[derive(Default)]
struct Events(Mutex<Vec<AgentEvent>>);
impl AgentSink for Events {
    fn emit(&self, e: AgentEvent) {
        self.0.lock().push(e);
    }
}

struct Host {
    approvals: AtomicUsize,
    approve: bool,
}
#[async_trait::async_trait]
impl ToolHost for Host {
    async fn approve(&self, _tool: &str, _summary: &str, detail: &Value) -> Option<Value> {
        self.approvals.fetch_add(1, Ordering::SeqCst);
        self.approve.then(|| detail.clone())
    }
}

fn sse(chunks: &[Value]) -> String {
    let mut s = String::new();
    for c in chunks {
        s.push_str(&format!("data: {c}\n\n"));
    }
    s.push_str("data: [DONE]\n\n");
    s
}

fn tool_call(id: &str, name: &str, args: Value) -> String {
    sse(&[json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id, "function": {"name": name, "arguments": args.to_string()}}]}, "finish_reason": "tool_calls"}]})])
}

/// Mock server: responds based on how many tool results the request contains.
async fn mock_openai(requests: Arc<Mutex<Vec<Value>>>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let body = loop {
                    let n = s.read(&mut tmp).await.unwrap();
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len: usize = text[..h]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + len {
                            break String::from_utf8_lossy(&buf[h + 4..h + 4 + len]).to_string();
                        }
                    }
                    if n == 0 {
                        break String::new();
                    }
                };
                let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                requests.lock().push(req.clone());
                let msgs = req["messages"].as_array().cloned().unwrap_or_default();
                let tool_results: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "tool").collect();
                let out = match tool_results.len() {
                    0 => tool_call("c1", "search_schema", json!({"query": "orders revenue by country"})),
                    1 => tool_call("c2", "run_query", json!({"sql": "select country, amount from orders", "purpose": "Get order amounts"})),
                    2 => {
                        let last = tool_results[1]["content"].as_str().unwrap_or("");
                        let rid = last.split("result_id=").nth(1).and_then(|r| r.split_whitespace().next()).unwrap_or("").to_string();
                        tool_call("c3", "query_result", json!({"result_id": rid, "sql": "select country, sum(amount) total from result group by 1 order by 2 desc"}))
                    }
                    _ => sse(&[
                        json!({"choices": [{"delta": {"content": "Vietnam leads "}}]}),
                        json!({"choices": [{"delta": {"content": "with 300."}, "finish_reason": "stop"}]}),
                        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 8}}),
                    ]),
                };
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

struct Fixture {
    engine: Arc<QueryEngine>,
    hub: Arc<EventHub>,
    ws: Arc<Workspace>,
    secrets: Arc<dyn SecretStore>,
    conn: String,
    provider: String,
    _dir: tempdir_like::Dir,
}

mod tempdir_like {
    pub struct Dir(pub std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

async fn fixture(port: u16, policy: AiPolicy) -> Fixture {
    let dir = std::env::temp_dir().join(format!("databrain-ai-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("shop.db");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "create table customers(id integer primary key, name text, email text);
             create table orders(id integer primary key, customer_id integer references customers(id), country text, amount real);
             insert into customers(id, name, email) values (1,'An','a@x.io'),(2,'Binh','b@x.io'),(3,'Chi','c@x.io');
             insert into orders(customer_id, country, amount) values (1,'Vietnam',100),(2,'Vietnam',200),(3,'Japan',150);",
        )
        .unwrap();
    }
    let mut reg = ConnectorRegistry::new();
    reg.register(Arc::new(databrain_connector_sqlite::SqliteConnector));
    let ws = Arc::new(Workspace::open_in_memory().unwrap());
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
    let hub = EventHub::new(Arc::new(NullSink));
    let engine = QueryEngine::new(reg, ws.clone(), secrets.clone(), Arc::new(ResultStore::new()), hub.clone());
    let mut cfg = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    cfg.file_path = Some(db.to_string_lossy().into());
    let conn = ws
        .save_connection(ConnectionProfile {
            id: String::new(),
            name: "Shop".into(),
            config: cfg,
            color: None,
            env: EnvTag::Dev,
            folder_id: None,
            has_secret: false,
            ai_policy: policy,
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    let provider = ws
        .save_ai_provider(AiProviderRecord {
            id: String::new(),
            kind: "openai_compatible".into(),
            name: "Mock".into(),
            config: serde_json::to_value(ProviderConfig { base_url: Some(format!("http://127.0.0.1:{port}/v1")), default_model: Some("mock-1".into()), ..Default::default() }).unwrap(),
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    secrets.set(&SecretRef::for_ai_provider(&provider.id), &"sk-test".to_string().into()).unwrap();
    // Index knowledge.
    let r = databrain_ai::knowledge::index_connection(&engine, &conn.id, None, &|_, _, _| {}, &Default::default()).await.unwrap();
    assert_eq!(r.objects, 2);
    Fixture { engine, hub, ws, secrets, conn: conn.id, provider: provider.id, _dir: tempdir_like::Dir(dir) }
}

fn req(f: &Fixture) -> AgentRequest {
    AgentRequest {
        session_id: None,
        connection_id: f.conn.clone(),
        provider_id: Some(f.provider.clone()),
        model: None,
        message: "Which country has the most revenue?".into(),
        mode: Mode::Chat,
        context: UiContext::default(),
    }
}

#[tokio::test]
async fn agent_answers_with_tools_and_approval() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    let agent = Agent { engine: f.engine.clone(), hub: f.hub.clone(), secrets: f.secrets.clone(), max_steps: 8 };
    let host = Arc::new(Host { approvals: AtomicUsize::new(0), approve: true });
    let events = Arc::new(Events::default());
    let text = agent.run(req(&f), "r1".into(), host.clone(), events.clone(), Default::default()).await.unwrap();
    assert_eq!(text, "Vietnam leads with 300.");
    assert_eq!(host.approvals.load(Ordering::SeqCst), 1, "run_query needs approval by default");

    let reqs = requests.lock().clone();
    assert_eq!(reqs.len(), 4);
    // Schema context was retrieved into the system prompt.
    let system = reqs[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("main.orders") && system.contains("FK (customer_id) -> main.customers(id)"), "{system}");
    assert!(reqs[0]["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "run_query"));
    // Raw rows were NOT sent (share_result_rows=false) but aggregates were.
    let run_result = reqs[2]["messages"].as_array().unwrap().iter().rfind(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_string();
    assert!(run_result.contains("returned 3 rows") && run_result.contains("Raw rows are not shared"), "{run_result}");
    assert!(!run_result.contains("Vietnam\t100"));
    let agg = reqs[3]["messages"].as_array().unwrap().iter().rfind(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_string();
    assert!(agg.contains("Vietnam\t300"), "{agg}");
    assert_eq!(reqs[0]["model"], "mock-1");

    // History records the AI origin; conversation persisted; audit written.
    let h = f.ws.list_history(&HistoryQuery::default()).unwrap();
    assert!(h.iter().any(|e| e.origin == Origin::Ai));
    let sessions = f.ws.list_ai_sessions(Some(&f.conn), 10).unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(f.ws.list_ai_messages(&sessions[0].id).unwrap().len() >= 8);
    assert!(f.ws.list_audit(20).unwrap().iter().any(|a| a.tool == "run_query" && a.decision == "approved"));
    let evs = events.0.lock();
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::ToolFinished { tool, .. } if tool == "query_result")));
    assert!(matches!(evs.last(), Some(AgentEvent::Finished { .. })));
}

#[tokio::test]
async fn denied_approval_and_blocked_writes() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    let host = Arc::new(Host { approvals: AtomicUsize::new(0), approve: false });
    let ctx = databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: f.ws.get_connection(&f.conn).unwrap(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host,
        cancel: Default::default(),
        results: Mutex::new(vec![]),
    };
    let o = ctx.call("run_query", &json!({"sql": "select 1"})).await;
    assert!(o.content.contains("declined"), "{}", o.content);
    let o = ctx.call("run_query", &json!({"sql": "delete from orders"})).await;
    assert!(o.content.starts_with("ERROR") && o.content.contains("not allowed"), "{}", o.content);
    // Nothing was deleted.
    let out = f
        .engine
        .run_and_wait(&f.hub, RunRequest { connection_id: f.conn.clone(), tab_id: "t".into(), sql: "select count(*) from orders".into(), base_offset: 0, row_limit: None, confirmed: true, origin: Origin::User, session_key: None, output_name: None }, None)
        .await
        .unwrap();
    assert_eq!(out[0].result.as_ref().unwrap().total_rows, 1);
    // MCP callers cannot trigger approval prompts.
    let mcp = databrain_ai::ToolContext { caller: databrain_ai::Caller::Mcp, ..ctx };
    assert!(mcp.call("run_query", &json!({"sql": "select 1"})).await.content.contains("requires approval"));
    // Auto-read policy runs without prompts.
    let mut p = f.ws.get_connection(&f.conn).unwrap();
    p.ai_policy.run_query = RunQueryPolicy::AutoRead;
    let auto = databrain_ai::ToolContext { profile: p, caller: databrain_ai::Caller::Mcp, ..mcp };
    let o = auto.call("run_query", &json!({"sql": "select country from orders"})).await;
    assert!(o.content.contains("returned 3 rows"), "{}", o.content);
    let o = auto.call("describe_table", &json!({"table": "orders"})).await;
    assert!(o.content.contains("customer_id") && !o.content.contains("Referenced by"));
    let o = auto.call("describe_table", &json!({"table": "customers"})).await;
    assert!(o.content.contains("Referenced by: main.orders"), "{}", o.content);
}

#[tokio::test]
async fn mcp_server_lists_and_calls_tools() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests).await;
    let f = fixture(port, AiPolicy { mcp_enabled: true, run_query: RunQueryPolicy::AutoRead, ..Default::default() }).await;
    let server = databrain_ai::mcp::McpServer { engine: f.engine.clone(), hub: f.hub.clone() };
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "list_connections", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "run_query", "arguments": {"connection": "shop", "sql": "select count(*) n from orders"}}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "run_query", "arguments": {"connection": "Shop", "sql": "drop table orders"}}}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "nope"}),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    let mut out = Vec::new();
    server.serve(tokio::io::BufReader::new(input.as_bytes()), &mut out).await.unwrap();
    let resps: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(resps.len(), 6, "notification gets no response");
    assert_eq!(resps[0]["result"]["serverInfo"]["name"], "databrain");
    let tools = resps[1]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "search_schema" && t["inputSchema"]["required"][0] == "connection"));
    assert!(!tools.iter().any(|t| t["name"] == "write_editor"));
    assert!(resps[2]["result"]["content"][0]["text"].as_str().unwrap().contains("Shop"));
    assert!(resps[3]["result"]["content"][0]["text"].as_str().unwrap().contains("returned 1 rows"));
    assert_eq!(resps[4]["result"]["isError"], true);
    assert_eq!(resps[5]["error"]["code"], -32601);
}

// ------------------------------------------------------------------ Kiro (fake kiro-cli)

fn fake_kiro() -> Option<String> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-kiro-cli");
    (cfg!(unix) && std::process::Command::new("python3").arg("--version").output().is_ok()).then(|| p.to_string_lossy().into_owned())
}

#[tokio::test]
async fn kiro_provider_drives_databrain_tools_over_acp_and_mcp() {
    let Some(cli) = fake_kiro() else { return eprintln!("skipped: needs python3") };
    let f = fixture(1, AiPolicy::default()).await;
    let agents = f._dir.0.join("kiro-agents");
    let log = f._dir.0.join("fake-kiro.log");
    let rec = f
        .ws
        .save_ai_provider(AiProviderRecord {
            id: String::new(),
            kind: "kiro".into(),
            name: "Kiro".into(),
            config: serde_json::to_value(ProviderConfig {
                base_url: Some(cli),
                agents_dir: Some(agents.to_string_lossy().into()),
                cli_env: [("FAKE_KIRO_LOG".to_string(), log.to_string_lossy().into_owned())].into_iter().collect(),
                ..Default::default()
            })
            .unwrap(),
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    let agent = Agent { engine: f.engine.clone(), hub: f.hub.clone(), secrets: f.secrets.clone(), max_steps: 8 };
    let events = Arc::new(Events::default());
    let host = Arc::new(Host { approvals: AtomicUsize::new(0), approve: true });
    let mut r = req(&f);
    r.provider_id = Some(rec.id.clone());

    // No API key stored yet → clear auth error before anything runs.
    let e = agent.run(r.clone(), "run0".into(), host.clone(), events.clone(), Default::default()).await.unwrap_err();
    assert!(e.to_string().contains("API key"), "{e}");

    f.secrets.set(&SecretRef::for_ai_provider(&rec.id), &"ksk_test".to_string().into()).unwrap();
    let text = agent.run(r, "run1".into(), host, events.clone(), Default::default()).await.unwrap();
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(text.contains("customers") && text.contains("orders"), "{text}\n{logged}");

    // What the fake Kiro observed.
    let lines: Vec<Value> = std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let spawn = lines.iter().find(|l| l.get("argv").is_some()).unwrap();
    assert_eq!(spawn["argv"], json!(["acp", "--agent", "databrain-sql"]));
    assert_eq!(spawn["has_key"], json!(true));
    let facts = lines.iter().find(|l| l.get("tools").is_some()).unwrap();
    assert_eq!(facts["context_block"], json!(true));
    assert_eq!(facts["no_token"], json!(401));
    assert_eq!(facts["origin"], json!(403));
    assert_eq!(facts["perm_databrain"], json!("allow"));
    assert_eq!(facts["perm_shell"], json!("deny"));
    assert_eq!(facts["fs_refused"], json!(true));
    assert!(facts["tools"].as_array().unwrap().iter().any(|t| t == "run_query"));

    // Managed agent config restricts Kiro to DataBrain's tools.
    let agent_cfg: Value = serde_json::from_str(&std::fs::read_to_string(agents.join("databrain-sql.json")).unwrap()).unwrap();
    assert_eq!(agent_cfg["tools"], json!(["@databrain"]));

    // UI saw the DataBrain tool card and streamed text; the answer is persisted.
    let evs = events.0.lock().clone();
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::ToolFinished { tool, .. } if tool == "list_tables")));
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::TextDelta { .. })));
    let sid = evs.iter().find_map(|e| match e { AgentEvent::Finished { session_id, .. } => Some(session_id.clone()), _ => None }).unwrap();
    let msgs = f.ws.list_ai_messages(&sid).unwrap();
    assert_eq!(msgs.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(), vec!["user", "assistant"]);
    assert_eq!(f.ws.get_setting(&format!("kiro_session:{sid}")).unwrap(), Some(json!("kiro-sess-1")));

    // Models come from the ACP session.
    let (p, _, _) = databrain_ai::provider_for(&rec, f.secrets.clone()).unwrap();
    let models: Vec<String> = p.list_models().await.unwrap().into_iter().map(|m| m.id).collect();
    assert_eq!(models, vec!["auto", "claude-sonnet-4.5"]);
}


/// Live: `DATABRAIN_KIRO_LIVE=1 KIRO_API_KEY=ksk_… cargo test -p databrain-ai --test agent_e2e kiro_live -- --nocapture`
/// Uses the real kiro-cli and consumes a few Kiro credits.
#[tokio::test]
async fn kiro_live() {
    if std::env::var("DATABRAIN_KIRO_LIVE").is_err() {
        return;
    }
    let key = std::env::var("KIRO_API_KEY").expect("KIRO_API_KEY");
    let f = fixture(1, AiPolicy { run_query: RunQueryPolicy::AutoRead, ..Default::default() }).await;
    let rec = f
        .ws
        .save_ai_provider(AiProviderRecord {
            id: String::new(),
            kind: "kiro".into(),
            name: "Kiro".into(),
            config: serde_json::to_value(ProviderConfig { default_model: Some("claude-haiku-4.5".into()), ..Default::default() }).unwrap(),
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    f.secrets.set(&SecretRef::for_ai_provider(&rec.id), &key.into()).unwrap();
    let agent = Agent { engine: f.engine.clone(), hub: f.hub.clone(), secrets: f.secrets.clone(), max_steps: 8 };
    let events = Arc::new(Events::default());
    let host = Arc::new(Host { approvals: AtomicUsize::new(0), approve: true });
    let mut r = req(&f);
    r.provider_id = Some(rec.id);
    r.message = "Using run_query, compute total order amount per country. Answer with the top country and its total.".into();
    let text = agent.run(r, "live".into(), host, events.clone(), Default::default()).await.unwrap();
    let tools: Vec<String> = events.0.lock().iter().filter_map(|e| match e { AgentEvent::ToolFinished { tool, .. } => Some(tool.clone()), _ => None }).collect();
    eprintln!("tools: {tools:?}\nanswer: {text}");
    assert!(tools.iter().any(|t| t == "run_query"), "{tools:?}");
    assert!(text.contains("Vietnam") && text.contains("300"), "{text}");
}

// ------------------------------------------------------------------ outputs: handles, @mentions, cross-connection policy

#[tokio::test]
async fn ai_tools_use_output_handles_with_source_policies() {
    use databrain_connector_core::external::ExternalTablesSlot;
    let f = fixture(1, AiPolicy::default()).await;
    // Engine whose DuckDB sessions can see outputs.
    let slot = ExternalTablesSlot::new();
    let mut reg = ConnectorRegistry::new();
    reg.register(Arc::new(databrain_connector_sqlite::SqliteConnector));
    reg.register(Arc::new(databrain_connector_duckdb::DuckdbConnector::with_outputs(slot.clone())));
    let engine = QueryEngine::new(reg, f.ws.clone(), f.secrets.clone(), Arc::new(ResultStore::new()), f.hub.clone());
    slot.set(engine.external_tables());

    // A second connection to the same data, with AI disabled.
    let mut closed = f.ws.get_connection(&f.conn).unwrap();
    closed.id = String::new();
    closed.name = "Closed".into();
    closed.ai_policy = AiPolicy { ai_enabled: false, ..Default::default() };
    let closed = f.ws.save_connection(closed).unwrap();
    // Open connection shares result rows.
    let mut open = f.ws.get_connection(&f.conn).unwrap();
    open.ai_policy.share_result_rows = true;
    let open = f.ws.save_connection(open).unwrap();

    let run = |conn: String, sql: &'static str, name: Option<&'static str>| {
        let (engine, hub) = (engine.clone(), f.hub.clone());
        async move {
            let r = RunRequest { connection_id: conn, tab_id: uuid::Uuid::new_v4().to_string(), sql: sql.into(), base_offset: 0, row_limit: None, confirmed: true, origin: Origin::User, session_key: None, output_name: name.map(Into::into) };
            engine.run_and_wait(&hub, r, None).await.unwrap()
        }
    };
    run(open.id.clone(), "select country, sum(amount) total from orders group by 1", Some("by_country")).await; // r1
    run(closed.id.clone(), "select country, count(*) n from orders group by 1", Some("secret")).await; // r2

    let ctx = databrain_ai::ToolContext {
        engine: engine.clone(),
        hub: f.hub.clone(),
        profile: open.clone(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve: true }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
    };
    // Handles work wherever a result id was accepted.
    let o = ctx.call("result_summary", &json!({"result_id": "by_country"})).await;
    assert!(o.content.contains("country") && o.content.contains("First 2 rows"), "{}", o.content);
    let o = ctx.call("query_result", &json!({"result_id": "r1", "sql": "select country from result order by total desc limit 1"})).await;
    assert!(o.content.contains("Vietnam"), "{}", o.content);
    // Outputs from a connection with AI disabled are invisible and blocked.
    let o = ctx.call("list_outputs", &json!({})).await;
    assert!(o.content.contains("results.by_country") && !o.content.contains("secret"), "{}", o.content);
    let o = ctx.call("query_result", &json!({"result_id": "secret", "sql": "select * from result"})).await;
    assert!(o.content.contains("AI is disabled"), "{}", o.content);
    let o = ctx.call("query_outputs", &json!({"sql": "select * from results.by_country a join results.secret b using (country)"})).await;
    assert!(o.content.contains("AI is disabled"), "{}", o.content);
    // DuckDB over outputs (a self-join stands in for two sources).
    let o = ctx.call("query_outputs", &json!({"sql": "select a.country, a.total from results.by_country a join results.r1 b using (country) order by 2 desc"})).await;
    assert!(o.content.contains("2 rows") && o.content.contains("Vietnam\t300"), "{}", o.content);
    let o = ctx.call("query_outputs", &json!({"sql": "create table x as select 1"})).await;
    assert!(o.content.starts_with("ERROR"), "{}", o.content);
    // MCP: an MCP-enabled connection cannot read outputs of a connection
    // that is not shared with MCP.
    let mut shared = f.ws.get_connection(&f.conn).unwrap();
    shared.id = String::new();
    shared.name = "Shared".into();
    shared.ai_policy = AiPolicy { mcp_enabled: true, share_result_rows: true, ..Default::default() };
    let shared = f.ws.save_connection(shared).unwrap();
    let mcp = databrain_ai::ToolContext { caller: databrain_ai::Caller::Mcp, profile: shared, ..ctx };
    let o = mcp.call("query_outputs", &json!({"sql": "select * from results.by_country"})).await;
    assert!(o.content.contains("not shared with MCP"), "{}", o.content);
}

/// Live: `DATABRAIN_KIRO_LIVE=1 cargo test -p databrain-ai --test agent_e2e kiro_live_status -- --nocapture`
#[tokio::test]
async fn kiro_live_status() {
    if std::env::var("DATABRAIN_KIRO_LIVE").is_err() {
        return;
    }
    use databrain_ai::kiro::KiroProvider;
    use databrain_ai::providers::{KeySource, ProviderAuth};
    let key = std::env::var("KIRO_API_KEY").expect("KIRO_API_KEY");
    let cfg = ProviderConfig { auth: ProviderAuth::ApiKey, ..Default::default() };
    let st = KiroProvider::new(&cfg, KeySource::Inline(key)).unwrap().status().await;
    eprintln!("api key: signed_in={} type={:?} identity_set={} msg={:?}", st.signed_in, st.account_type, st.identity.is_some(), st.message);
    assert!(st.signed_in, "{:?}", st.message);
    let bad = KiroProvider::new(&cfg, KeySource::Inline("ksk_invalid".into())).unwrap().status().await;
    eprintln!("bad key: signed_in={} type={:?} msg={:?}", bad.signed_in, bad.account_type, bad.message);
    assert!(!bad.signed_in && bad.message.as_deref().unwrap_or("").contains("did not accept"));
    let cfg = ProviderConfig { auth: ProviderAuth::KiroBrowser, ..Default::default() };
    let br = KiroProvider::new(&cfg, KeySource::None).unwrap().status().await;
    eprintln!("browser: signed_in={} type={:?} msg={:?}", br.signed_in, br.account_type, br.message);
    if !br.signed_in {
        assert!(br.message.as_deref().unwrap_or("").contains("Sign in with browser"));
    }
}

// ------------------------------------------------------------------ knowledge: plan, scope, cancel

#[tokio::test]
async fn knowledge_index_plan_scope_and_cancel() {
    use databrain_ai::knowledge::{index_connection, plan, scope_matches};
    let f = fixture(1, AiPolicy::default()).await;
    let mut reg = ConnectorRegistry::new();
    reg.register(Arc::new(databrain_connector_duckdb::DuckdbConnector::new()));
    let engine = QueryEngine::new(reg, f.ws.clone(), f.secrets.clone(), Arc::new(ResultStore::new()), f.hub.clone());
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = databrain_connector_core::ConnectionConfig::new(databrain_connector_core::ConnectorKind::Duckdb, databrain_auth::AuthMethod::None);
    cfg.file_path = Some(dir.path().join("lake.duckdb").to_string_lossy().into());
    let mut p = f.ws.get_connection(&f.conn).unwrap();
    p.id = String::new();
    p.name = "Lake".into();
    p.config = cfg;
    let conn = f.ws.save_connection(p).unwrap().id;
    let mut ddl = String::new();
    for i in 0..60 {
        ddl.push_str(&format!("create schema s{i:02}; create table s{i:02}.t{i:02}(id integer, name varchar);"));
    }
    let r = RunRequest { connection_id: conn.clone(), tab_id: "setup".into(), sql: ddl, base_offset: 0, row_limit: None, confirmed: true, origin: Origin::User, session_key: None, output_name: None };
    engine.run_and_wait(&f.hub, r, None).await.unwrap();

    // Plan: one listing + counts, flagged as large.
    let pl = plan(&engine, &conn).await.unwrap();
    assert!(pl.large && pl.schemas.len() >= 60, "{} schemas", pl.schemas.len());
    assert_eq!(pl.schemas.iter().find(|s| s.name == "lake.s07").unwrap().objects, Some(1));
    assert_eq!(pl.total_objects, Some(60));

    // Explicit scope; catalog wildcard.
    assert!(scope_matches(&["lake.*".into()], "lake.s01") && !scope_matches(&["lake.*".into()], "lakehouse.s01"));
    let two = ["lake.s01".to_string(), "LAKE.S02".to_string()];
    let r = index_connection(&engine, &conn, Some(&two), &|_, _, _| {}, &Default::default()).await.unwrap();
    assert_eq!((r.schemas, r.objects, r.cancelled), (2, 2, false));
    assert_eq!(f.ws.kn_count(&conn).unwrap(), 2);

    // Cancel after the first batch: finished schemas stay indexed.
    let cancel = tokio_util::sync::CancellationToken::new();
    let calls = AtomicUsize::new(0);
    let c2 = cancel.clone();
    let progress = move |_: &str, _: usize, _: usize| {
        if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            c2.cancel();
        }
    };
    let all = ["lake.*".to_string()];
    let r = index_connection(&engine, &conn, Some(&all), &progress, &cancel).await.unwrap();
    assert!(r.cancelled);
    assert!(r.objects >= 25 && r.objects < 60, "{}", r.objects);
    let st = f.ws.kn_state(&conn).unwrap().unwrap();
    assert!(st.error.as_deref().unwrap_or("").contains("cancelled"), "{:?}", st.error);
    assert_eq!(st.objects as usize, f.ws.kn_count(&conn).unwrap() as usize);

    // Bigger batches: fewer metadata calls (progress fires once per batch).
    let mut p = f.ws.get_connection(&conn).unwrap();
    p.ai_policy.index_batch = 100;
    f.ws.save_connection(p).unwrap();
    let calls = AtomicUsize::new(0);
    let r = index_connection(&engine, &conn, Some(&all), &|_, _, _| { calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst); }, &Default::default()).await.unwrap();
    assert_eq!((r.objects, r.cancelled), (60, false));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2, "one batch + final");
    assert_eq!(plan(&engine, &conn).await.unwrap().batch, 100);

    // Already cancelled before start.
    let done = tokio_util::sync::CancellationToken::new();
    done.cancel();
    assert!(matches!(index_connection(&engine, &conn, None, &|_, _, _| {}, &done).await, Err(databrain_ai::AiError::Cancelled)));
}
