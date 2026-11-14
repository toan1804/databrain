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
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id, "function": {"name": name, "arguments": args.to_string()}}]}, "finish_reason": "tool_calls"}]}),
    ])
}

/// Mock server: responds based on how many tool results the request contains.
async fn mock_openai(requests: Arc<Mutex<Vec<Value>>>) -> u16 {
    mock_server(requests, |req| {
        let msgs = req["messages"].as_array().cloned().unwrap_or_default();
        let tool_results: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "tool").collect();
        match tool_results.len() {
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
        }
    })
    .await
}

/// Mock OpenAI-compatible server: records each request body and answers
/// with the SSE body `respond` returns for it.
async fn mock_server(requests: Arc<Mutex<Vec<Value>>>, respond: impl Fn(&Value) -> String + Send + Sync + 'static) -> u16 {
    let respond = Arc::new(respond);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let (requests, respond) = (requests.clone(), respond.clone());
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
                let out = respond(&req);
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
            config: serde_json::to_value(ProviderConfig {
                base_url: Some(format!("http://127.0.0.1:{port}/v1")),
                default_model: Some("mock-1".into()),
                ..Default::default()
            })
            .unwrap(),
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
        provider_id: None,
    };
    let o = ctx.call("run_query", &json!({"sql": "select 1"})).await;
    assert!(o.content.contains("declined"), "{}", o.content);
    let o = ctx.call("run_query", &json!({"sql": "delete from orders"})).await;
    assert!(o.content.starts_with("ERROR") && o.content.contains("not allowed"), "{}", o.content);
    // Nothing was deleted.
    let out = f
        .engine
        .run_and_wait(
            &f.hub,
            RunRequest {
                connection_id: f.conn.clone(),
                tab_id: "t".into(),
                sql: "select count(*) from orders".into(),
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin: Origin::User,
                session_key: None,
                output_name: None,
                params: Default::default(),
                output_masks: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(out[0].result.as_ref().unwrap().total_rows, 1);
    // MCP with a client that can't show DataBrain's question: the client's
    // own tool approval isn't DataBrain's (clients auto-approve tools), so
    // what DataBrain would ask about is refused…
    struct NoElicitation;
    #[async_trait::async_trait]
    impl ToolHost for NoElicitation {
        fn can_ask(&self) -> bool {
            false
        }
        async fn approve(&self, _: &str, _: &str, _: &Value) -> Option<Value> {
            unreachable!("can_ask is false")
        }
    }
    let mcp = databrain_ai::ToolContext { caller: databrain_ai::Caller::Mcp, host: Arc::new(NoElicitation), ..ctx };
    let o = mcp.call("run_query", &json!({"sql": "select 1"})).await;
    assert!(o.content.starts_with("ERROR") && o.content.contains("elicitation"), "{}", o.content);
    // The refusal tells the agent what the user can change.
    assert!(o.content.contains("Edit connection") && o.content.contains("Accept the MCP client's approval"), "{}", o.content);
    assert!(f.ws.list_audit(50).unwrap().iter().any(|a| a.tool == "run_query" && a.decision == "blocked"));
    // …unless the connection accepts the client's approval.
    let mut trusting = f.ws.get_connection(&f.conn).unwrap();
    trusting.ai_policy.mcp_client_approves = true;
    let mcp = databrain_ai::ToolContext { profile: trusting.clone(), ..mcp };
    let o = mcp.call("run_query", &json!({"sql": "select 1"})).await;
    assert!(o.content.contains("returned 1 row"), "{}", o.content);
    // What the policy forbids is refused either way.
    let o = mcp.call("run_query", &json!({"sql": "delete from orders"})).await;
    assert!(o.content.starts_with("ERROR") && o.content.contains("not allowed"), "{}", o.content);
    assert!(f.ws.list_audit(50).unwrap().iter().any(|a| a.tool == "run_query" && a.decision == "approved by MCP client"));
    // A client that could ask isn't asked either: with "Ask me every time"
    // (not auto-read), the client's approval is enough.
    struct MustNotAsk;
    #[async_trait::async_trait]
    impl ToolHost for MustNotAsk {
        async fn approve(&self, _: &str, _: &str, _: &Value) -> Option<Value> {
            panic!("the connection accepts the client's approval: DataBrain doesn't ask")
        }
    }
    assert_eq!(trusting.ai_policy.run_query, RunQueryPolicy::Ask);
    let mcp = databrain_ai::ToolContext { host: Arc::new(MustNotAsk), ..mcp };
    let o = mcp.call("run_query", &json!({"sql": "select country from orders"})).await;
    assert!(o.content.contains("returned 3 rows"), "{}", o.content);
    // Jobs: only in DataBrain's assistant.
    for tool in ["create_job", "update_job", "run_job"] {
        let o = mcp.call(tool, &json!({"name": "x", "job": "x", "nodes": []})).await;
        assert!(o.content.starts_with("ERROR") && o.content.contains("only available in DataBrain's assistant"), "{tool}: {}", o.content);
    }
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
    let mut resps: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(resps.len(), 6, "notification gets no response");
    // Requests are handled concurrently: answers come as they finish, matched by id.
    resps.sort_by_key(|r| r["id"].as_i64());
    assert_eq!(resps[0]["result"]["serverInfo"]["name"], "databrain");
    let tools = resps[1]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "search_schema" && t["inputSchema"]["required"][0] == "connection"));
    assert!(!tools.iter().any(|t| t["name"] == "write_editor"));
    // Jobs are read over MCP but created, changed and run only in the app.
    assert!(tools.iter().any(|t| t["name"] == "list_jobs") && tools.iter().any(|t| t["name"] == "get_job"));
    for app_only in ["create_job", "update_job", "run_job"] {
        assert!(!tools.iter().any(|t| t["name"] == app_only), "{app_only} is not offered over MCP");
    }
    // Annotations: read tools can be auto-approved, run_query can't.
    let ann = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap()["annotations"].clone();
    assert_eq!((ann("search_schema")["readOnlyHint"].clone(), ann("search_schema")["destructiveHint"].clone()), (json!(true), json!(false)));
    assert_eq!((ann("run_query")["readOnlyHint"].clone(), ann("run_query")["destructiveHint"].clone()), (json!(false), json!(true)));
    assert!(tools.iter().all(|t| t["annotations"]["openWorldHint"] == json!(false)), "every tool is annotated");
    assert!(resps[2]["result"]["content"][0]["text"].as_str().unwrap().contains("Shop"));
    assert!(resps[3]["result"]["content"][0]["text"].as_str().unwrap().contains("returned 1 rows"));
    assert_eq!(resps[4]["result"]["isError"], true);
    assert_eq!(resps[5]["error"]["code"], -32601);
}

// ------------------------------------------------------------------ MCP elicitation

/// A client that supports elicitation: DataBrain asks the user through it,
/// and only an explicit "approve" runs the query.
#[tokio::test]
async fn mcp_asks_the_user_through_elicitation() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests).await;
    // run_query = Ask (the default): every query needs the user.
    let f = fixture(port, AiPolicy { mcp_enabled: true, ..Default::default() }).await;
    let server = Arc::new(databrain_ai::mcp::McpServer { engine: f.engine.clone(), hub: f.hub.clone() });
    let (client, server_io) = tokio::io::duplex(1 << 16);
    let (server_r, server_w) = tokio::io::split(server_io);
    let srv = server.clone();
    let serving = tokio::spawn(async move { srv.serve(BufReader::new(server_r), server_w).await });
    let (client_r, mut client_w) = tokio::io::split(client);
    let mut lines = BufReader::new(client_r).lines();
    async fn send(w: &mut (impl tokio::io::AsyncWrite + Unpin), v: Value) {
        w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }
    send(&mut client_w, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {"elicitation": {}}, "clientInfo": {"name": "t", "version": "1"}}})).await;
    let init: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(init["id"], 1);

    // Approved: the server asks, the user approves, the query runs.
    send(&mut client_w, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "run_query", "arguments": {"connection": "shop", "sql": "select count(*) n from orders"}}})).await;
    let ask: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ask["method"], "elicitation/create", "{ask}");
    let message = ask["params"]["message"].as_str().unwrap();
    assert!(message.contains("run_query") && message.contains("select count(*) n from orders"), "{message}");
    assert_eq!(ask["params"]["requestedSchema"]["required"][0], "approve");
    send(&mut client_w, json!({"jsonrpc": "2.0", "id": ask["id"], "result": {"action": "accept", "content": {"approve": true}}})).await;
    let done: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(done["id"], 2);
    assert_eq!(done["result"]["isError"], false, "{done}");
    assert!(done["result"]["content"][0]["text"].as_str().unwrap().contains("returned 1 row"));

    // Accepting the form without ticking "approve", or declining, doesn't run it.
    for answer in [json!({"action": "accept", "content": {"approve": false}}), json!({"action": "decline"})] {
        send(
            &mut client_w,
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "run_query", "arguments": {"connection": "shop", "sql": "select 1"}}}),
        )
        .await;
        let ask: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ask["method"], "elicitation/create");
        send(&mut client_w, json!({"jsonrpc": "2.0", "id": ask["id"], "result": answer})).await;
        let done: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(done["result"]["isError"], true, "{done}");
        assert!(done["result"]["content"][0]["text"].as_str().unwrap().contains("declined"), "{done}");
    }

    // The client closes while DataBrain waits for the user: declined, and the server stops.
    send(
        &mut client_w,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "run_query", "arguments": {"connection": "shop", "sql": "select 2"}}}),
    )
    .await;
    let ask: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ask["method"], "elicitation/create");
    client_w.shutdown().await.unwrap();
    let done: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(done["result"]["isError"], true, "{done}");
    tokio::time::timeout(std::time::Duration::from_secs(10), serving).await.unwrap().unwrap().unwrap();
    let audit = f.ws.list_audit(50).unwrap();
    assert_eq!(audit.iter().filter(|a| a.tool == "run_query" && a.decision == "approved").count(), 1);
    assert_eq!(audit.iter().filter(|a| a.tool == "run_query" && a.decision == "denied").count(), 3);
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
    let rec =
        f.ws.save_ai_provider(AiProviderRecord {
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
    let sid = evs
        .iter()
        .find_map(|e| match e {
            AgentEvent::Finished { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .unwrap();
    let msgs = f.ws.list_ai_messages(&sid).unwrap();
    assert_eq!(msgs.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(), vec!["user", "assistant"]);
    assert_eq!(f.ws.get_setting(&format!("kiro_session:{sid}")).unwrap(), Some(json!("kiro-sess-1")));

    // Models come from the ACP session.
    let (p, _, _) = databrain_ai::provider_for(&rec, f.secrets.clone()).unwrap();
    let models: Vec<String> = p.list_models().await.unwrap().into_iter().map(|m| m.id).collect();
    assert_eq!(models, vec!["auto", "claude-sonnet-4.5"]);
}

/// Test data can use Kiro: one plain prompt per call, with no tools offered,
/// and the answer read like a model's. Kiro may pick its own model.
#[tokio::test]
async fn kiro_writes_test_data_without_tools() {
    use databrain_query_engine::testdata::{AiAsk, AiColumn};
    let Some(cli) = fake_kiro() else { return eprintln!("skipped: needs python3") };
    let f = fixture(1, AiPolicy::default()).await;
    let log = f._dir.0.join("fake-kiro.log");
    let env = [("FAKE_KIRO_LOG", log.to_string_lossy().into_owned()), ("FAKE_KIRO_REPLY", r#"Here: {"email": "%@corp.vn"}"#.into())];
    let rec =
        f.ws.save_ai_provider(AiProviderRecord {
            id: String::new(),
            kind: "kiro".into(),
            name: "Kiro".into(),
            config: serde_json::to_value(ProviderConfig {
                base_url: Some(cli),
                agents_dir: Some(f._dir.0.join("kiro-agents").to_string_lossy().into()),
                cli_env: env.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                ..Default::default()
            })
            .unwrap(),
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    f.secrets.set(&SecretRef::for_ai_provider(&rec.id), &"ksk_test".to_string().into()).unwrap();
    let ask = AiAsk {
        schema: "main".into(),
        name: "customers".into(),
        table: "\"main\".\"customers\"".into(),
        dialect: "sqlite".into(),
        checks: vec![],
        columns: vec![AiColumn {
            name: "email".into(),
            data_type: "text".into(),
            nullable: true,
            unique: false,
            max_chars: None,
            choices: vec![],
            hint: "email".into(),
            pattern: None,
        }],
        others: vec![],
        rows: 1,
    };
    let ai = databrain_ai::testdata::TestDataAi { provider_id: Some(rec.id.clone()), model: None, all_columns: true, rows: 1 };
    let got = databrain_ai::testdata::suggest_patterns(&f.ws, f.secrets.clone(), &f.conn, &ai, &ask, &Default::default()).await.unwrap();
    assert_eq!(got.get("email").map(String::as_str), Some("%@corp.vn"));
    let lines: Vec<Value> = std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    // No --model: Kiro chose ("auto"); no tool server was offered.
    assert_eq!(lines.iter().find(|l| l.get("argv").is_some()).unwrap()["argv"], json!(["acp", "--agent", "databrain-sql"]));
    let plain = lines.iter().find(|l| l.get("plain").is_some()).expect("a plain prompt");
    assert!(plain["prompt"].as_str().unwrap().contains("email"), "{plain}");
}

/// Live: `KIRO_API_KEY=ksk_… cargo test -p databrain-ai --test agent_e2e kiro_live -- --ignored --nocapture`
/// Uses the real kiro-cli and consumes a few Kiro credits.
#[ignore = "live: needs kiro-cli and KIRO_API_KEY, uses Kiro credits (cargo test -- --ignored)"]
#[tokio::test]
async fn kiro_live() {
    let key = std::env::var("KIRO_API_KEY").expect("KIRO_API_KEY");
    let f = fixture(1, AiPolicy { run_query: RunQueryPolicy::AutoRead, ..Default::default() }).await;
    let rec =
        f.ws.save_ai_provider(AiProviderRecord {
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
    let tools: Vec<String> = events
        .0
        .lock()
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolFinished { tool, .. } => Some(tool.clone()),
            _ => None,
        })
        .collect();
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
            let r = RunRequest {
                connection_id: conn,
                tab_id: uuid::Uuid::new_v4().to_string(),
                sql: sql.into(),
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin: Origin::User,
                session_key: None,
                output_name: name.map(Into::into),
                params: Default::default(),
                output_masks: None,
            };
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
        provider_id: None,
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
    let o = ctx
        .call("query_outputs", &json!({"sql": "select a.country, a.total from results.by_country a join results.r1 b using (country) order by 2 desc"}))
        .await;
    assert!(o.content.contains("2 rows") && o.content.contains("Vietnam\t300"), "{}", o.content);
    let o = ctx.call("query_outputs", &json!({"sql": "create table x as select 1"})).await;
    assert!(o.content.starts_with("ERROR"), "{}", o.content);
    // run_query on the current (SQLite) connection with results.<name> runs on Results (DuckDB) instead.
    let o = ctx.call("run_query", &json!({"sql": "select country, total from results.by_country order by total desc"})).await;
    assert!(o.content.contains("Vietnam\t300"), "{}", o.content);
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

    // A result larger than memory (on disk): the local aggregate and the
    // statistics say why they don't run (a long read from disk) and point
    // to query_outputs, which reads the files.
    let spill = tempfile::tempdir().unwrap();
    engine.results().set_spill_root(spill.path().join("spill"));
    engine.outputs().set_budget(0); // the 16 MB floor: 8 MB per result
    let big = run(
        open.id.clone(),
        "with recursive n(i) as (select 1 union all select i + 1 from n where i < 1000000) select i, 'row number ' || i as s from n",
        Some("big"),
    )
    .await;
    assert!(big[0].result.as_ref().is_some_and(|r| r.on_disk), "{:?}", big[0].result);
    let ctx = databrain_ai::ToolContext { caller: databrain_ai::Caller::Agent, profile: open.clone(), ..mcp };
    let o = ctx.call("query_result", &json!({"result_id": "big", "sql": "select count(*) from result"})).await;
    assert!(o.content.contains("on disk") && o.content.contains("long-running") && o.content.contains("query_outputs"), "{}", o.content);
    let o = ctx.call("result_summary", &json!({"result_id": "big"})).await;
    assert!(o.content.contains("long-running") && !o.content.contains("distinct="), "{}", o.content);
    // query_outputs runs over every row.
    let o = ctx.call("query_outputs", &json!({"sql": "select count(*) as n from results.big"})).await;
    assert!(o.content.contains("1000000"), "{}", o.content);
}

/// Sensitive columns of an output stay masked in query_outputs and run_query
/// on DuckDB whatever the SQL does with them: a CTE column list renaming
/// them, `SELECT *`, a cast whose error would quote the value.
#[tokio::test]
async fn output_columns_stay_masked_in_duckdb() {
    use databrain_connector_core::external::ExternalTablesSlot;
    let f = fixture(1, AiPolicy::default()).await;
    let slot = ExternalTablesSlot::new();
    let mut reg = ConnectorRegistry::new();
    reg.register(Arc::new(databrain_connector_sqlite::SqliteConnector));
    reg.register(Arc::new(databrain_connector_duckdb::DuckdbConnector::with_outputs(slot.clone())));
    let engine = QueryEngine::new(reg, f.ws.clone(), f.secrets.clone(), Arc::new(ResultStore::new()), f.hub.clone());
    slot.set(engine.external_tables());
    let mut conn = f.ws.get_connection(&f.conn).unwrap();
    conn.ai_policy.share_result_rows = true;
    conn.ai_policy.pii_columns = vec!["customers.email".into()];
    let conn = f.ws.save_connection(conn).unwrap();
    let r = RunRequest {
        connection_id: conn.id.clone(),
        tab_id: "user".into(),
        sql: "select id, name, email from customers order by id".into(),
        base_offset: 0,
        row_limit: None,
        confirmed: true,
        origin: Origin::User,
        session_key: None,
        output_name: Some("people".into()),
        params: Default::default(),
        output_masks: None,
    };
    engine.run_and_wait(&f.hub, r, None).await.unwrap();
    let ctx = databrain_ai::ToolContext {
        engine: engine.clone(),
        hub: f.hub.clone(),
        profile: conn.clone(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve: true }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    for sql in [
        "with c(a, b, x) as (select * from results.people) select x from c order by a",
        "select * from results.people order by id",
        "select email as contact, upper(email) u from results.people order by id",
        "select string_agg(t::varchar, ';') from results.people t",
    ] {
        let o = ctx.call("query_outputs", &json!({"sql": sql})).await;
        assert!(!o.content.contains("@x.io") && !o.content.contains("@X.IO"), "{sql}: {}", o.content);
        assert!(o.content.contains("***"), "{sql}: {}", o.content);
        let o = ctx.call("run_query", &json!({"sql": sql})).await;
        assert!(!o.content.contains("@x.io") && !o.content.contains("@X.IO"), "run_query {sql}: {}", o.content);
    }
    // An error message can't quote a value either.
    let o = ctx.call("query_outputs", &json!({"sql": "with c(a, b, x) as (select * from results.people) select cast(x as integer) from c"})).await;
    assert!(!o.content.contains("@x.io"), "{}", o.content);
    // Other columns still work (a value computed from the masked one is
    // masked again when the rows are shown).
    let o = ctx.call("query_outputs", &json!({"sql": "select max(id) n, max(name) m from results.people"})).await;
    assert!(o.content.contains("3\tChi"), "{}", o.content);
    // The user's own grid keeps the real values.
    let user = engine.outputs().resolve("people").unwrap();
    let rs = engine.results().get(&user.result_id).unwrap();
    let page = rs.lock().page(&Default::default(), 0, 5).unwrap();
    assert_eq!(page.rows[0][2].as_deref(), Some("a@x.io"));
}

/// Table access rules: run_query, sample_rows, outputs the user made from a
/// denied table, and jobs the AI builds all respect them; MCP too.
#[tokio::test]
async fn table_access_rules() {
    use databrain_connector_core::external::ExternalTablesSlot;
    let rules = AiPolicy {
        run_query: RunQueryPolicy::AutoRead,
        share_sample_values: true,
        share_result_rows: true,
        mcp_enabled: true,
        allow_write: true,
        read_deny: vec!["'%customers%'".into()],
        write_allow: vec!["main.scratch%".into()],
        ..Default::default()
    };
    let f = fixture(1, rules).await;
    let slot = ExternalTablesSlot::new();
    let mut reg = ConnectorRegistry::new();
    reg.register(Arc::new(databrain_connector_sqlite::SqliteConnector));
    reg.register(Arc::new(databrain_connector_duckdb::DuckdbConnector::with_outputs(slot.clone())));
    let engine = QueryEngine::new(reg, f.ws.clone(), f.secrets.clone(), Arc::new(ResultStore::new()), f.hub.clone());
    slot.set(engine.external_tables());
    let ctx = databrain_ai::ToolContext {
        engine: engine.clone(),
        hub: f.hub.clone(),
        profile: f.ws.get_connection(&f.conn).unwrap(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve: true }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    let o = ctx.call("run_query", &json!({"sql": "select country, sum(amount) from orders group by 1"})).await;
    assert!(o.content.contains("returned 2 rows"), "{}", o.content);
    for sql in [
        "select * from customers",
        "select o.* from orders o join main.customers c on c.id = o.customer_id",
        "select * from orders where customer_id in (select id from customers)",
    ] {
        let o = ctx.call("run_query", &json!({"sql": sql})).await;
        assert!(o.content.starts_with("ERROR") && o.content.contains("main.customers"), "{sql}: {}", o.content);
    }
    // Writes: only main.scratch*.
    let o = ctx.call("run_query", &json!({"sql": "create table scratch_a as select * from orders"})).await;
    assert!(!o.content.starts_with("ERROR"), "{}", o.content);
    let o = ctx.call("run_query", &json!({"sql": "delete from orders where id = 1"})).await;
    assert!(o.content.contains("may only write tables matching 'main.scratch%'"), "{}", o.content);
    // sample_rows and MCP.
    let o = ctx.call("get_sample_rows", &json!({"table": "main.customers"})).await;
    // Denied reads as missing (the same words as a table that isn't there).
    assert!(o.content.starts_with("ERROR") && o.content.contains("not available to the AI"), "{}", o.content);
    let mcp = databrain_ai::ToolContext {
        caller: databrain_ai::Caller::Mcp,
        profile: ctx.profile.clone(),
        engine: engine.clone(),
        hub: f.hub.clone(),
        session_id: Some("m".into()),
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve: true }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    assert!(mcp.call("run_query", &json!({"sql": "select email from customers"})).await.content.starts_with("ERROR"));

    // The user queried customers in a tab: the AI can't read that output, nor
    // one combined from it on DuckDB.
    let run = |conn: String, sql: &'static str, name: &'static str| {
        let (engine, hub) = (engine.clone(), f.hub.clone());
        async move {
            let r = RunRequest {
                connection_id: conn,
                tab_id: uuid::Uuid::new_v4().to_string(),
                sql: sql.into(),
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin: Origin::User,
                session_key: None,
                output_name: Some(name.into()),
                params: Default::default(),
                output_masks: None,
            };
            let o = engine.run_and_wait(&hub, r, None).await.unwrap();
            assert!(o.iter().all(|x| x.error.is_none()), "{:?}", o[0].error);
        }
    };
    run(f.conn.clone(), "select name, email from customers", "people").await;
    let duck = engine.results_connection().unwrap().id;
    run(duck, "select count(*) n from results.people", "people_count").await;
    for r in ["people", "people_count"] {
        let o = ctx.call("query_result", &json!({"result_id": r, "sql": "select * from result"})).await;
        assert!(o.content.starts_with("ERROR") && o.content.contains("can't be shared"), "{r}: {}", o.content);
    }
    let o = ctx.call("list_outputs", &json!({})).await;
    assert!(!o.content.contains("results.people"), "{}", o.content);

    // Jobs the AI builds: a query step on a denied table, or a load into a
    // table outside the write rules, are refused before asking.
    let o = ctx.call("create_job", &json!({"name": "J1", "steps": [{"name": "j_cust", "connection": "Shop", "sql": "select * from customers"}]})).await;
    assert!(o.content.starts_with("ERROR") && o.content.contains("step j_cust"), "{}", o.content);
    let o = ctx
        .call("create_job", &json!({"name": "J2", "steps": [{"name": "j_ord", "connection": "Shop", "sql": "select * from orders"}, {"name": "j_load", "action": "load", "after": ["j_ord"], "target_connection": "Shop", "target_table": "main.orders_copy"}]}))
        .await;
    assert!(o.content.starts_with("ERROR") && o.content.contains("step j_load"), "{}", o.content);
    let o = ctx
        .call("create_job", &json!({"name": "J3", "steps": [{"name": "j_ord3", "connection": "Shop", "sql": "select * from orders"}, {"name": "j_load3", "action": "load", "after": ["j_ord3"], "target_connection": "Shop", "target_table": "main.scratch_copy"}]}))
        .await;
    assert!(!o.content.starts_with("ERROR"), "{}", o.content);
}

/// Live: `cargo test -p databrain-ai --test agent_e2e kiro_live_status -- --ignored --nocapture`
#[ignore = "live: needs a signed-in kiro-cli (cargo test -- --ignored)"]
#[tokio::test]
async fn kiro_live_status() {
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
    let r = RunRequest {
        connection_id: conn.clone(),
        tab_id: "setup".into(),
        sql: ddl,
        base_offset: 0,
        row_limit: None,
        confirmed: true,
        origin: Origin::User,
        session_key: None,
        output_name: None,
        params: Default::default(),
        output_masks: None,
    };
    engine.run_and_wait(&f.hub, r, None).await.unwrap();
    // The setup tab's session holds the file; Windows lets one session open it.
    engine.disconnect(&conn);

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
    let r = index_connection(
        &engine,
        &conn,
        Some(&all),
        &|_, _, _| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        },
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!((r.objects, r.cancelled), (60, false));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2, "one batch + final");
    assert_eq!(plan(&engine, &conn).await.unwrap().batch, 100);

    // Incremental: nothing changed → no schema is re-read.
    let calls = AtomicUsize::new(0);
    let r = index_connection(
        &engine,
        &conn,
        Some(&all),
        &|_, _, _| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        },
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!((r.objects, r.skipped, r.changed), (60, 61, 0), "60 schemas and the empty main, all unchanged");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "no batch, final only");
    // One table altered → only its schema is re-read.
    let alter = RunRequest {
        connection_id: conn.clone(),
        tab_id: "setup".into(),
        sql: "alter table s07.t07 add column email varchar".into(),
        base_offset: 0,
        row_limit: None,
        confirmed: true,
        origin: Origin::User,
        session_key: None,
        output_name: None,
        params: Default::default(),
        output_masks: None,
    };
    // Each DuckDB session opens the file on its own (one writer at a time).
    engine.disconnect(&conn);
    engine.run_and_wait(&f.hub, alter, None).await.unwrap();
    engine.disconnect(&conn);
    let r = index_connection(&engine, &conn, Some(&all), &|_, _, _| {}, &Default::default()).await.unwrap();
    assert_eq!((r.objects, r.skipped, r.changed), (60, 60, 1), "the other 59 schemas and main are skipped");
    assert!(f.ws.kn_get(&conn, "lake.s07.t07").unwrap().unwrap().columns.iter().any(|c| c.name == "email"));
    // Full rebuild ignores fingerprints.
    let r = databrain_ai::knowledge::index_connection_with(&engine, &conn, Some(&all), true, &|_, _, _| {}, &Default::default()).await.unwrap();
    assert_eq!((r.objects, r.skipped), (60, 0));
    // Clearing the index forgets fingerprints too.
    f.ws.kn_clear(&conn).unwrap();
    let r = index_connection(&engine, &conn, Some(&all), &|_, _, _| {}, &Default::default()).await.unwrap();
    assert_eq!((r.objects, r.skipped), (60, 0));

    // Already cancelled before start.
    let done = tokio_util::sync::CancellationToken::new();
    done.cancel();
    assert!(matches!(index_connection(&engine, &conn, None, &|_, _, _| {}, &done).await, Err(databrain_ai::AiError::Cancelled)));
}

#[tokio::test]
async fn knowledge_note_tools_add_list_update() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    let ctx = databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: f.ws.get_connection(&f.conn).unwrap(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve: true }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    let o = ctx.call("add_knowledge_note", &json!({"target": "main.orders", "note": "status 2 = shipped"})).await;
    assert!(o.content.contains("review"), "{}", o.content);
    let o = ctx.call("add_knowledge_note", &json!({"note": "STATUS 2 =  shipped"})).await;
    assert!(o.content.contains("identical note already exists"), "no duplicates: {}", o.content);
    // A user note the AI later corrects.
    let user =
        f.ws.kn_save_note(databrain_workspace::KnNote {
            id: String::new(),
            connection_id: f.conn.clone(),
            target: None,
            body: "Revenue excludes tax".into(),
            author: "user".into(),
            status: databrain_workspace::NoteStatus::Approved,
            created_at: 0,
            replaces: None,
        })
        .unwrap();
    let o = ctx.call("list_knowledge_notes", &json!({"filter": "revenue"})).await;
    assert!(o.content.contains(&user.id) && !o.content.contains("shipped"), "{}", o.content);
    let o = ctx.call("update_knowledge_note", &json!({"id": user.id, "note": "Revenue excludes tax and refunds"})).await;
    assert!(o.content.contains("review"), "{}", o.content);
    let notes = f.ws.kn_notes(&f.conn).unwrap();
    let upd = notes.iter().find(|n| n.replaces.as_deref() == Some(user.id.as_str())).expect("update proposal");
    assert_eq!(upd.status, databrain_workspace::NoteStatus::Proposed);
    assert!(notes.iter().any(|n| n.id == user.id), "user note kept until the update is approved");
    // Auto-approve: saved directly and the old text is replaced.
    let mut p = f.ws.get_connection(&f.conn).unwrap();
    p.ai_policy.auto_approve_notes = true;
    let auto = databrain_ai::ToolContext { profile: p, ..ctx };
    let o = auto.call("update_knowledge_note", &json!({"id": user.id, "note": "Revenue excludes tax, refunds and shipping"})).await;
    assert!(o.content.contains("saved"), "{}", o.content);
    let notes = f.ws.kn_notes(&f.conn).unwrap();
    assert!(notes.iter().all(|n| n.id != user.id));
    assert!(notes.iter().any(|n| n.body.ends_with("shipping") && n.status == databrain_workspace::NoteStatus::Approved));
    assert!(auto.call("update_knowledge_note", &json!({"id": "nope", "note": "x"})).await.content.starts_with("ERROR"));
}

#[tokio::test]
async fn note_targets_must_be_real_tables() {
    use databrain_ai::knowledge::check_note_target;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    // Live lookups (index may be empty): canonical names, several tables, a column.
    assert_eq!(check_note_target(&f.engine, &f.conn, "ORDERS and main.customers").await.unwrap(), "main.orders and main.customers");
    assert_eq!(check_note_target(&f.engine, &f.conn, "orders & customers.EMAIL").await.unwrap(), "main.orders & main.customers.email");
    let e = check_note_target(&f.engine, &f.conn, "orders & nope_table").await.unwrap_err();
    assert!(e.contains("nope_table") && !e.contains("orders,"), "{e}");
    assert!(check_note_target(&f.engine, &f.conn, "orders.no_such_column").await.is_err());
    // Tables seen in the explorer / completion count without asking the
    // database (`cached_only` only exists in that cache, so the database can't have answered).
    use databrain_connector_core::{ColumnInfo, DbObject, ObjectKind};
    f.ws.meta_add_objects(
        &f.conn,
        &[DbObject { schema: "main".into(), name: "cached_only".into(), kind: ObjectKind::Table, comment: None, row_estimate: None }],
    )
    .unwrap();
    f.ws.meta_put_columns(
        &f.conn,
        "main",
        "cached_only",
        &[ColumnInfo { name: "Code".into(), data_type: "text".into(), nullable: true, is_primary_key: false, default: None, comment: None }],
    )
    .unwrap();
    assert_eq!(check_note_target(&f.engine, &f.conn, "CACHED_ONLY & main.cached_only.code").await.unwrap(), "main.cached_only & main.cached_only.Code");
    let ctx = databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: f.ws.get_connection(&f.conn).unwrap(),
        session_id: None,
        caller: databrain_ai::Caller::Agent,
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve: true }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    let o = ctx.call("add_knowledge_note", &json!({"target": "orders or ghost", "note": "x"})).await;
    assert!(o.content.starts_with("ERROR") && o.content.contains("ghost"), "{}", o.content);
    let o = ctx.call("add_knowledge_note", &json!({"target": "orders & customers", "note": "join on customer_id"})).await;
    assert!(!o.content.starts_with("ERROR"), "{}", o.content);
    assert!(f.ws.kn_notes(&f.conn).unwrap().iter().any(|n| n.target.as_deref() == Some("main.orders & main.customers")));
}

/// The assistant reads and creates jobs across connections (with approval).
#[tokio::test]
async fn creates_a_job_across_connections() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    let profile = f.ws.get_connection(&f.conn).unwrap();
    let ctx = |approve: bool| databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: profile.clone(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    let c = ctx(true);
    let conns = c.call("list_connections", &json!({})).await.content;
    assert!(conns.contains(&profile.name) && conns.contains("Results"), "{conns}");
    // Schema tools take another connection by name.
    let d = c.call("describe_table", &json!({"table": "orders", "connection": profile.name})).await.content;
    assert!(!d.starts_with("ERROR"), "{d}");
    // run_query on another connection, in its dialect.
    let other = |name: &str, ai: bool| ConnectionProfile {
        id: String::new(),
        name: name.into(),
        ai_policy: AiPolicy { ai_enabled: ai, ..AiPolicy::default() },
        ..profile.clone()
    };
    f.ws.save_connection(other("Shop copy", true)).unwrap();
    f.ws.save_connection(other("Secret", false)).unwrap();
    let q = c.call("run_query", &json!({"sql": "select count(*) n from customers", "connection": "shop copy"})).await.content;
    assert!(q.starts_with("Ran (on \"Shop copy\")") && q.contains("returned 1 rows"), "{q}");
    assert!(c.call("run_query", &json!({"sql": "select 1", "connection": "Secret"})).await.content.contains("AI is disabled"));
    assert!(!c.call("list_connections", &json!({})).await.content.contains("Secret"));
    assert!(c.call("run_query", &json!({"sql": "select 1", "connection": "nope"})).await.content.contains("no connection"));
    let job = json!({
        "name": "Revenue by country",
        "steps": [
            {"name": "ai_orders", "connection": profile.name, "sql": "select country, amount from orders"},
            {"name": "ai_by_country", "connection": "Results", "sql": "select country, sum(amount) total from results.ai_orders group by 1"},
            {"name": "ai_file", "action": "export", "after": ["ai_by_country"], "format": "csv"}
        ]
    });
    assert!(ctx(false).call("create_job", &job).await.content.contains("declined"));
    assert!(f.ws.list_jobs().unwrap().is_empty());
    let o = c.call("create_job", &job).await;
    assert!(o.content.starts_with("Created job"), "{}", o.content);
    let saved = f.ws.list_jobs().unwrap();
    assert_eq!(saved.len(), 1);
    let j = f.ws.get_job(&saved[0].id).unwrap();
    assert_eq!(j.nodes.len(), 3);
    assert_eq!(j.edges.len(), 2, "results.ai_orders links the steps");
    let g = c.call("get_job", &json!({"job": "revenue by country"})).await.content;
    assert!(g.contains("ai_by_country = query on \"Results") && g.contains("(after ai_orders)"), "{g}");
    assert!(c.call("list_jobs", &json!({})).await.content.contains("Revenue by country"));
    // A second job can't take the same output names.
    assert!(c.call("create_job", &job).await.content.contains("already"));
    assert!(g.contains("Spec (edit and send to update_job)"), "{g}");

    // Fix the job: change a step's SQL, drop the export, add a schedule.
    let fix = json!({
        "job": "Revenue by country",
        "steps": [
            {"name": "ai_orders", "connection": profile.name, "sql": "select country, amount from orders where amount > 0"},
            {"name": "ai_by_country", "connection": "Results", "sql": "select country, sum(amount) total from results.ai_orders group by 1"}
        ],
        "schedule": {"daily_at": "07:00"}
    });
    assert!(ctx(false).call("update_job", &fix).await.content.contains("declined"));
    assert_eq!(f.ws.get_job(&j.id).unwrap().nodes.len(), 3, "unchanged when declined");
    let o = c.call("update_job", &fix).await;
    assert!(
        o.content.starts_with("Updated job") && o.content.contains("changes step ai_orders") && o.content.contains("removes step ai_file"),
        "{}",
        o.content
    );
    let after = f.ws.get_job(&j.id).unwrap();
    assert_eq!(after.nodes.len(), 2);
    assert_eq!(after.nodes[0].id, j.nodes[0].id, "the step kept its id");
    assert!(after.schedule.enabled && after.schedule.at == "07:00");
    // Running a job needs the app's runner and the user's approval.
    assert!(ctx(false).call("run_job", &json!({"job": "Revenue by country"})).await.content.contains("declined"));
    let r = c.call("run_job", &json!({"job": "Revenue by country"})).await.content;
    assert!(r.contains("only available in the DataBrain app"), "{r}");
    assert!(c.call("run_job", &json!({"job": "Revenue by country", "steps": ["nope"]})).await.content.contains("has no step"));
    // Same again: nothing to change.
    assert!(c.call("update_job", &fix).await.content.starts_with("Nothing changes"));
}

/// Jobs the assistant builds, changes or runs follow its policy like its
/// own queries do (runs don't prompt, so it is checked before).
#[tokio::test]
async fn jobs_follow_the_ai_policy() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    let profile = f.ws.get_connection(&f.conn).unwrap();
    let host = Arc::new(Host { approvals: AtomicUsize::new(0), approve: true });
    let c = databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: profile.clone(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host: host.clone(),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    let save = |name: &str, policy: AiPolicy, env: EnvTag| {
        f.ws.save_connection(ConnectionProfile { id: String::new(), name: name.into(), ai_policy: policy, env, ..profile.clone() }).unwrap()
    };
    let writer = AiPolicy { allow_write: true, ..AiPolicy::default() };
    save("Writer", writer.clone(), EnvTag::Dev);
    save("Prod writer", writer, EnvTag::Prod);
    let create = |steps: Value| json!({"name": "J", "steps": steps});
    let refused = |o: &str, why: &str| o.starts_with("ERROR") && o.contains(why);

    // Writes in a query step need allow_write, like run_query.
    let o = c.call("create_job", &create(json!([{"name": "w1", "connection": "Shop", "sql": "delete from orders"}]))).await.content;
    assert!(refused(&o, "step w1") && o.contains("not allowed"), "{o}");
    // On the Results DuckDB too (its own policy: no writes by default).
    let o = c.call("create_job", &create(json!([{"name": "w2", "connection": "Results", "sql": "create table x as select 1"}]))).await.content;
    assert!(refused(&o, "step w2") && o.contains("not allowed"), "{o}");
    // The SQL of load and export steps runs there and may only read.
    let o = c
        .call(
            "create_job",
            &create(json!([
                {"name": "w3", "connection": "Shop", "sql": "select * from orders"},
                {"name": "w4", "action": "export", "after": ["w3"], "sql": "copy (select 1) to '/tmp/x.csv'"}
            ])),
        )
        .await
        .content;
    assert!(refused(&o, "may only read"), "{o}");
    // A load writes: the target must let the AI write, never production.
    let load = |target: &str, before: Option<&str>| {
        create(json!([
            {"name": "src", "connection": "Shop", "sql": "select * from orders"},
            {"name": "dst", "action": "load", "after": ["src"], "target_connection": target, "target_table": "orders_copy", "before_sql": before}
        ]))
    };
    let o = c.call("create_job", &load("Shop", None)).await.content;
    assert!(refused(&o, "step dst") && o.contains("writes are not allowed"), "{o}");
    let o = c.call("create_job", &load("Prod writer", None)).await.content;
    assert!(refused(&o, "production"), "{o}");
    assert!(f.ws.list_jobs().unwrap().is_empty(), "nothing refused was saved");
    assert_eq!(host.approvals.load(Ordering::SeqCst), 0, "refused before asking the user");
    let o = c.call("create_job", &load("Writer", Some("drop table orders"))).await.content;
    assert!(o.starts_with("Created job"), "allowed with allow_write (and approval): {o}");

    // A job set up by hand with a write the AI may not make: the AI can't run or change it.
    let mut job = f.ws.get_job(&f.ws.list_jobs().unwrap()[0].id).unwrap();
    job.nodes[0].sql = "update orders set amount = 0".into();
    f.ws.save_job(job).unwrap();
    let o = c.call("run_job", &json!({"job": "J"})).await.content;
    assert!(refused(&o, "step src") && !o.contains("only available in the DataBrain app"), "refused before the run starts: {o}");
    let o = c.call("run_job", &json!({"job": "J", "steps": ["dst"]})).await.content;
    assert!(o.contains("only available in the DataBrain app"), "only the selected steps are checked: {o}");
    // Load steps' before/after SQL follows the target's policy too.
    let o = c.call("update_job", &json!({"job": "J", "steps": [
        {"name": "src", "connection": "Shop", "sql": "select * from orders"},
        {"name": "dst", "action": "load", "after": ["src"], "target_connection": "Writer", "target_table": "orders_copy", "after_sql": "select 1; delete from customers"}
    ]})).await.content;
    assert!(!o.starts_with("ERROR"), "{o}");
    f.ws.save_connection(ConnectionProfile {
        ai_policy: AiPolicy::default(),
        ..f.ws.list_connections().unwrap().into_iter().find(|p| p.name == "Writer").unwrap()
    })
    .unwrap();
    let o = c.call("run_job", &json!({"job": "J", "steps": ["dst"]})).await.content;
    assert!(refused(&o, "step dst"), "the target no longer allows writes: {o}");
}

/// On a database without a read-only session mode, `SELECT dbo.write_fn()`
/// can write: where the AI may not write, only the user may run it, even
/// with automatic reads, and never an MCP client.
#[tokio::test]
async fn functions_that_could_write_need_the_user() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    let shop = f.ws.get_connection(&f.conn).unwrap();
    let mut config = shop.config.clone();
    config.kind = ConnectorKind::Mssql;
    let policy = AiPolicy { run_query: RunQueryPolicy::AutoRead, mcp_enabled: true, ..AiPolicy::default() };
    let mssql = f.ws.save_connection(ConnectionProfile { id: String::new(), name: "Warehouse".into(), config, ai_policy: policy, ..shop.clone() }).unwrap();
    let ctx = |caller: databrain_ai::Caller, host: Arc<Host>| databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: mssql.clone(),
        session_id: Some("s".into()),
        caller,
        host,
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    let declines = Arc::new(Host { approvals: AtomicUsize::new(0), approve: false });
    let agent = ctx(databrain_ai::Caller::Agent, declines.clone());
    let mcp = ctx(databrain_ai::Caller::Mcp, declines.clone());
    let call = json!({"sql": "select dbo.write_fn()"});
    let o = mcp.call("run_query", &call).await.content;
    assert!(o.contains("dbo.write_fn") && o.contains("an MCP client can't"), "{o}");
    assert_eq!(declines.approvals.load(Ordering::SeqCst), 0);
    assert!(f.ws.list_audit(5).unwrap().iter().any(|a| a.tool == "run_query" && a.decision == "blocked"));
    let o = agent.call("run_query", &call).await.content;
    assert!(o.contains("declined"), "{o}");
    assert_eq!(declines.approvals.load(Ordering::SeqCst), 1, "asked although reads run automatically");
    // A job step calling it needs the user too (in the app; MCP can't create jobs at all).
    let job = json!({"name": "Fn", "steps": [{"name": "fn_step", "connection": "Warehouse", "sql": "select dbo.write_fn()"}]});
    let o = mcp.call("create_job", &job).await.content;
    assert!(o.contains("only available in DataBrain's assistant"), "{o}");
    let asked = declines.approvals.load(Ordering::SeqCst);
    let o = agent.call("create_job", &job).await.content;
    assert!(o.contains("declined"), "{o}");
    assert_eq!(declines.approvals.load(Ordering::SeqCst), asked + 1, "the user was asked");
    assert!(f.ws.list_jobs().unwrap().is_empty());
}

/// "Ask AI" about a connection test error: the conversation is filed under a
/// connection, but the assistant must not connect to it (that could start a
/// sign-in), read its schema, or run tools.
#[tokio::test]
async fn no_database_questions_never_connect() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    f.engine.disconnect(&f.conn);
    assert!(!f.engine.connected_ids().contains(&f.conn));
    let agent = Agent { engine: f.engine.clone(), hub: f.hub.clone(), secrets: f.secrets.clone(), max_steps: 8 };
    let host = Arc::new(Host { approvals: AtomicUsize::new(0), approve: true });
    let mut r = req(&f);
    r.message = "Why did the connection test fail? connection refused".into();
    r.context.no_database = true;
    agent.run(r, "r1".into(), host.clone(), Arc::new(Events::default()), Default::default()).await.unwrap();
    let reqs = requests.lock().clone();
    let system = reqs[0]["messages"][0]["content"].as_str().unwrap();
    assert!(!system.contains("main.orders"), "no schema in the prompt: {system}");
    assert!(reqs[0]["tools"].as_array().is_none_or(|t| t.is_empty()), "no tools offered");
    // The mock calls tools anyway: each one is refused.
    for q in &reqs[1..] {
        let last = q["messages"].as_array().unwrap().iter().rfind(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_string();
        assert!(last.contains("tools are off"), "{last}");
    }
    assert_eq!(host.approvals.load(Ordering::SeqCst), 0);
    assert!(!f.engine.connected_ids().contains(&f.conn), "the connection was not opened");
}

/// A table missing from the index is read from the database after approval,
/// added to the index (keeping the rest), and described right away.
#[tokio::test]
async fn index_tables_adds_missing_tables_after_approval() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    // The AI may use nothing on the connection.
    f.ws.kn_clear(&f.conn).unwrap();
    f.ws.ai_set_scope(&f.conn, &[]).unwrap();
    let ctx = |approve: bool| databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: f.ws.get_connection(&f.conn).unwrap(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    let missing = ctx(true).call("describe_table", &json!({"table": "orders"})).await;
    assert!(missing.content.contains("index_tables"), "{}", missing.content);
    // Declined: nothing indexed.
    let no = ctx(false).call("index_tables", &json!({"tables": ["orders"]})).await;
    assert!(no.content.contains("declined"), "{}", no.content);
    assert_eq!(f.ws.kn_count(&f.conn).unwrap(), 0);
    // Approved: orders is indexed (a wrong name is reported), described at once.
    let c = ctx(true);
    let o = c.call("index_tables", &json!({"tables": ["orders", "nope_table"], "reason": "revenue question"})).await;
    assert!(o.content.contains("Indexed 1 of 2") && o.content.contains("country") && o.content.contains("nope_table: not indexed"), "{}", o.content);
    assert_eq!(f.ws.kn_count(&f.conn).unwrap(), 1);
    let d = c.call("describe_table", &json!({"table": "orders"})).await;
    assert!(d.content.contains("customer_id"), "{}", d.content);
    // Indexing another keeps the first.
    c.call("index_tables", &json!({"tables": ["main.customers"]})).await;
    assert_eq!(f.ws.kn_count(&f.conn).unwrap(), 2);
    assert!(f.ws.kn_state(&f.conn).unwrap().is_some_and(|s| s.objects == 2));
}

/// A bare name in several schemas is read from the one the AI's scope
/// allows (not the explorer cache's first); a database that can't be
/// reached is said as such, not "not available".
#[tokio::test]
async fn index_tables_resolves_by_scope_and_says_why_a_read_failed() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_openai(requests.clone()).await;
    let f = fixture(port, AiPolicy::default()).await;
    f.ws.kn_clear(&f.conn).unwrap();
    // `hr.orders` (seen in the explorer, shorter schema: the cache's first
    // pick for `orders`) is excluded; `main.orders` isn't.
    let o = |schema: &str| databrain_connector_core::DbObject {
        schema: schema.into(),
        name: "orders".into(),
        kind: databrain_connector_core::ObjectKind::Table,
        comment: None,
        row_estimate: None,
    };
    f.ws.meta_add_objects(&f.conn, &[o("hr"), o("main")]).unwrap();
    f.ws.ai_put_rule(&f.conn, &databrain_workspace::ScopeRule { schema: "hr".into(), table: String::new(), allow: false }).unwrap();
    let cancel = Default::default();
    let r = databrain_ai::knowledge::index_tables(&f.engine, &f.conn, &["orders".into()], &cancel).await.unwrap();
    let got = r[0].1.as_ref().map(|o| format!("{}.{}", o.schema, o.name));
    assert_eq!(got.as_deref(), Ok("main.orders"));
    // Only the excluded one: refused, in the words for a missing table.
    let r = databrain_ai::knowledge::index_tables(&f.engine, &f.conn, &["hr.orders".into()], &cancel).await.unwrap();
    assert!(r[0].1.as_ref().unwrap_err().contains("not available"), "{:?}", r[0].1);

    // The database can't be opened (a folder where its file was).
    let mut p = f.ws.get_connection(&f.conn).unwrap();
    let broken = std::env::temp_dir().join(format!("databrain-ai-broken-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&broken).unwrap();
    p.config.file_path = Some(broken.to_string_lossy().into());
    f.ws.save_connection(p).unwrap();
    f.engine.disconnect(&f.conn);
    let r = databrain_ai::knowledge::index_tables(&f.engine, &f.conn, &["main.customers".into()], &cancel).await.unwrap();
    let e = r[0].1.as_ref().unwrap_err();
    assert!(e.starts_with("could not read `main.customers`") && !e.contains("not available"), "{e}");
    let _ = std::fs::remove_dir_all(&broken);
}

/// Joins written down in a document (or said by the user) are recorded
/// against the knowledge index: real tables, real columns. The assistant's
/// wait for review unless notes are saved directly; an MCP client's are
/// marked as such; a join the user rejected is not recorded again.
#[tokio::test]
async fn documented_joins_are_checked_reviewed_and_used() {
    let f = fixture(1, AiPolicy::default()).await;
    let ctx = databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: f.ws.get_connection(&f.conn).unwrap(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve: true }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    let join = |left: &str, lc: &[&str], right: &str, rc: &[&str], source: &str| json!({"left_table": left, "left_columns": lc, "right_table": right, "right_columns": rc, "source": source});
    // Checked: tables in the index, columns that exist, a source, matching sides.
    let o = ctx.call("add_knowledge_join", &join("orders", &["nope"], "customers", &["id"], "spec")).await;
    assert!(o.content.contains("has no column `nope`") && o.content.contains("customer_id"), "{}", o.content);
    let o = ctx.call("add_knowledge_join", &join("invoices", &["id"], "customers", &["id"], "spec")).await;
    assert!(o.content.contains("not available to the AI"), "{}", o.content);
    let o = ctx.call("add_knowledge_join", &join("orders", &["country"], "customers", &["name"], " ")).await;
    assert!(o.content.contains("source"), "{}", o.content);
    let o = ctx.call("add_knowledge_join", &join("orders", &["country", "id"], "customers", &["name"], "spec")).await;
    assert!(o.content.contains("same number of columns"), "{}", o.content);

    // Recorded (columns in any case), waiting for the user.
    let o = ctx.call("add_knowledge_join", &join("orders", &["COUNTRY"], "customers", &["Name"], "Shop data dictionary")).await;
    assert!(o.content.contains("main.orders.country = main.customers.name") && o.content.contains("review"), "{}", o.content);
    let o = ctx.call("describe_table", &json!({"table": "customers"})).await;
    assert!(!o.content.contains("documented"), "not given before review: {}", o.content);
    let j = f.ws.learned_joins(&f.conn).unwrap().into_iter().find(|j| j.documented.is_some()).unwrap();
    let doc = j.documented.clone().unwrap();
    assert_eq!((doc.by.as_str(), doc.state), ("ai", databrain_workspace::DocState::Proposed));
    // Approved: describe_table and find_join_path use it, with its source, with no query seen.
    f.ws.review_documented_join(j.id, true).unwrap();
    let o = ctx.call("describe_table", &json!({"table": "customers"})).await;
    assert!(o.content.contains("documented (Shop data dictionary)"), "{}", o.content);
    let o = ctx.call("find_join_path", &json!({"from": "orders", "to": "customers"})).await;
    assert!(o.content.contains("main.orders.country = main.customers.name  -- documented (Shop data dictionary)"), "{}", o.content);

    // Rejected by the user: not recorded again.
    f.ws.set_join_status(j.id, databrain_workspace::JoinStatus::Rejected).unwrap();
    let o = ctx.call("add_knowledge_join", &join("customers", &["name"], "orders", &["country"], "another doc")).await;
    assert!(o.content.contains("rejected"), "{}", o.content);

    // An MCP client on a connection that saves AI notes directly.
    let mut p = f.ws.get_connection(&f.conn).unwrap();
    p.ai_policy.auto_approve_notes = true;
    p.ai_policy.mcp_enabled = true;
    let p = f.ws.save_connection(p).unwrap();
    let mcp = databrain_ai::ToolContext { caller: databrain_ai::Caller::Mcp, profile: p, ..ctx };
    let o = mcp.call("add_knowledge_join", &join("orders", &["id"], "customers", &["id"], "the user")).await;
    assert!(o.content.contains("use it now"), "{}", o.content);
    let j = f.ws.learned_joins(&f.conn).unwrap().into_iter().find(|j| j.left_columns == ["id"]).unwrap();
    let doc = j.documented.unwrap();
    assert_eq!((doc.by.as_str(), doc.state, doc.source.as_str()), ("mcp", databrain_workspace::DocState::Approved, "the user"));
}

/// A reply with no text and no tool calls (a provider blocking it) is not
/// saved, the user is told why, and the conversation goes on. An empty turn
/// saved by an older version is left out of what is sent.
#[tokio::test]
async fn an_empty_reply_does_not_break_the_conversation() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let port = mock_server(requests.clone(), |req| {
        let msgs = req["messages"].as_array().cloned().unwrap_or_default();
        let last = msgs.last().and_then(|m| m["content"].as_str()).unwrap_or("").to_string();
        if last.starts_with("blocked") {
            sse(&[json!({"choices": [{"delta": {}, "finish_reason": "content_filter"}]})])
        } else {
            sse(&[json!({"choices": [{"delta": {"content": "Fine."}, "finish_reason": "stop"}]})])
        }
    })
    .await;
    let f = fixture(port, AiPolicy::default()).await;
    let agent = Agent { engine: f.engine.clone(), hub: f.hub.clone(), secrets: f.secrets.clone(), max_steps: 8 };
    let host = Arc::new(Host { approvals: AtomicUsize::new(0), approve: true });
    let mut r = req(&f);
    r.message = "blocked question".into();
    let events = Arc::new(Events::default());
    let e = agent.run(r.clone(), "r1".into(), host.clone(), events.clone(), Default::default()).await.unwrap_err();
    assert!(e.to_string().contains("content_filter"), "{e}");
    let sid = f.ws.list_ai_sessions(Some(&f.conn), 10).unwrap()[0].id.clone();
    let saved = f.ws.list_ai_messages(&sid).unwrap();
    assert_eq!(saved.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(), ["user"], "the empty reply was not saved");
    // An empty turn an older version saved.
    f.ws.add_ai_message(&sid, "assistant", &json!({"role": "assistant", "text": ""}), (None, None)).unwrap();
    r.session_id = Some(sid.clone());
    r.message = "next question".into();
    let text = agent.run(r, "r2".into(), host, events, Default::default()).await.unwrap();
    assert_eq!(text, "Fine.");
    let sent = requests.lock().last().cloned().unwrap();
    let roles: Vec<&str> = sent["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "user"], "{sent}");
}

/// Every tool that reads table facts sees only what the AI scope allows: a
/// table excluded by the user reads as missing, in the same words as one
/// that doesn't exist; an allowed one answers.
#[tokio::test]
async fn ai_tools_see_only_the_tables_the_scope_allows() {
    let f = fixture(1, AiPolicy { share_sample_values: true, ..Default::default() }).await;
    f.ws.ai_put_rule(&f.conn, &databrain_workspace::ScopeRule { schema: "main".into(), table: "customers".into(), allow: false }).unwrap();
    let ctx = databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: f.ws.get_connection(&f.conn).unwrap(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host: Arc::new(Host { approvals: AtomicUsize::new(0), approve: true }),
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: None,
    };
    let missing = |c: &str| c.starts_with("ERROR") && c.contains("not available to the AI");
    // describe_table
    let o = ctx.call("describe_table", &json!({"table": "orders"})).await;
    assert!(o.content.contains("customer_id") && !o.content.starts_with("ERROR"), "{}", o.content);
    let o = ctx.call("describe_table", &json!({"table": "customers"})).await;
    assert!(missing(&o.content), "{}", o.content);
    let o = ctx.call("describe_table", &json!({"table": "no_such_table"})).await;
    assert!(missing(&o.content), "{}", o.content);
    // list_tables
    let o = ctx.call("list_tables", &json!({})).await;
    assert!(o.content.contains("main.orders") && !o.content.contains("customers"), "{}", o.content);
    // search_schema
    let o = ctx.call("search_schema", &json!({"query": "customers email name"})).await;
    assert!(!o.content.contains("main.customers"), "{}", o.content);
    let o = ctx.call("search_schema", &json!({"query": "orders amount country"})).await;
    assert!(o.content.contains("main.orders"), "{}", o.content);
    // get_sample_rows
    let o = ctx.call("get_sample_rows", &json!({"table": "orders"})).await;
    assert!(!o.content.starts_with("ERROR"), "{}", o.content);
    let o = ctx.call("get_sample_rows", &json!({"table": "customers"})).await;
    assert!(missing(&o.content), "{}", o.content);
    // find_join_path
    let o = ctx.call("find_join_path", &json!({"from": "orders", "to": "customers"})).await;
    assert!(missing(&o.content), "{}", o.content);
    // add_knowledge_join
    let j = json!({"left_table": "orders", "left_columns": ["customer_id"], "right_table": "customers", "right_columns": ["id"], "source": "spec"});
    let o = ctx.call("add_knowledge_join", &j).await;
    assert!(missing(&o.content), "{}", o.content);
    // Notes: one about an excluded table isn't listed, and the AI can't probe
    // for one by targeting it.
    f.ws.kn_save_note(databrain_workspace::KnNote {
        id: String::new(),
        connection_id: f.conn.clone(),
        target: Some("main.orders & main.customers".into()),
        body: "orders.customer_id joins customers.id".into(),
        author: "user".into(),
        status: databrain_workspace::NoteStatus::Approved,
        created_at: 0,
        replaces: None,
    })
    .unwrap();
    let o = ctx.call("list_knowledge_notes", &json!({})).await;
    assert!(!o.content.contains("customers"), "{}", o.content);
    let o = ctx.call("add_knowledge_note", &json!({"note": "x", "target": "main.customers"})).await;
    assert!(missing(&o.content), "{}", o.content);
    let o = ctx.call("add_knowledge_note", &json!({"note": "x", "target": "main.no_such"})).await;
    assert!(missing(&o.content), "{}", o.content);
    // index_tables can't re-allow what the user excluded (approved or not).
    let o = ctx.call("index_tables", &json!({"tables": ["main.customers"]})).await;
    assert!(o.content.contains("not available to the AI") && o.content.contains("Indexed 0"), "{}", o.content);
    assert!(f.ws.ai_excluded(&f.conn, "main", "customers").unwrap());
    let o = ctx.call("describe_table", &json!({"table": "customers"})).await;
    assert!(missing(&o.content), "{}", o.content);
    // Allowed again: answered.
    f.ws.ai_delete_rule(&f.conn, "main", "customers").unwrap();
    let o = ctx.call("describe_table", &json!({"table": "customers"})).await;
    assert!(o.content.contains("email"), "{}", o.content);
}

// ------------------------------------------------------------------ each connection's own rules, wherever it is reached from

/// The result in the grid, for `query_result` / `result_summary`.
struct Grid(String);
#[async_trait::async_trait]
impl ToolHost for Grid {
    async fn approve(&self, _tool: &str, _summary: &str, detail: &Value) -> Option<Value> {
        Some(detail.clone())
    }
    fn current_result(&self) -> Option<String> {
        Some(self.0.clone())
    }
}

/// A connection's AI settings hold when it is reached from another
/// connection's conversation, through the grid's current result, or through
/// a job: allowed providers, "share metadata", AI off.
#[tokio::test]
async fn a_connections_rules_hold_wherever_it_is_reached_from() {
    let f = fixture(1, AiPolicy { run_query: RunQueryPolicy::AutoRead, share_result_rows: true, ..Default::default() }).await;
    let profile = f.ws.get_connection(&f.conn).unwrap();
    let other = |name: &str, policy: AiPolicy| {
        f.ws.save_connection(ConnectionProfile { id: String::new(), name: name.into(), ai_policy: policy, ..profile.clone() }).unwrap()
    };
    let auto = || AiPolicy { run_query: RunQueryPolicy::AutoRead, ..Default::default() };
    other("Local only", AiPolicy { allowed_providers: vec!["local-model".into()], ..auto() });
    let copy = other("Shop copy", auto());
    let closed = other("Closed", AiPolicy { ai_enabled: false, ..Default::default() });
    let ctx = |provider: &str, host: Arc<dyn ToolHost>| databrain_ai::ToolContext {
        engine: f.engine.clone(),
        hub: f.hub.clone(),
        profile: profile.clone(),
        session_id: Some("s".into()),
        caller: databrain_ai::Caller::Agent,
        host,
        cancel: Default::default(),
        results: Mutex::new(vec![]),
        provider_id: Some(provider.into()),
    };
    let yes = || -> Arc<dyn ToolHost> { Arc::new(Host { approvals: AtomicUsize::new(0), approve: true }) };

    // Allowed providers: a conversation on another provider can't read the
    // connection's tables or data through the `connection` argument.
    let cloud = ctx("cloud", yes());
    for (tool, args) in [
        ("run_query", json!({"sql": "select count(*) n from customers", "connection": "Local only"})),
        ("describe_table", json!({"table": "orders", "connection": "Local only"})),
        ("list_tables", json!({"connection": "Local only"})),
        ("search_schema", json!({"query": "orders", "connection": "Local only"})),
    ] {
        let o = cloud.call(tool, &args).await.content;
        assert!(o.starts_with("ERROR") && o.contains("doesn't allow this conversation's AI provider"), "{tool}: {o}");
    }
    // The provider it allows, and connections without a list, work.
    let o = ctx("local-model", yes()).call("run_query", &json!({"sql": "select count(*) n from customers", "connection": "Local only"})).await.content;
    assert!(o.contains("returned 1 rows"), "{o}");
    let o = cloud.call("run_query", &json!({"sql": "select count(*) n from customers", "connection": "Shop copy"})).await.content;
    assert!(o.contains("returned 1 rows"), "{o}");

    // Reads the assistant runs are capped by a trailing LIMIT, not a
    // subquery: unnamed and repeated columns, and two statements, run.
    let o = cloud
        .call("run_query", &json!({"sql": "select c.id, o.id, count(*) from customers c join orders o on o.customer_id = c.id group by 1, 2"}))
        .await
        .content;
    assert!(o.contains("returned 3 rows"), "{o}");
    let o = cloud.call("run_query", &json!({"sql": "select 1 as a; select 2 as b"})).await.content;
    assert!(o.contains("Statement 1 returned 1 rows") && o.contains("Statement 2 returned 1 rows"), "{o}");

    // "Share metadata" off: describe_table gives the name only, index_tables is refused.
    let mut names_only = profile.clone();
    names_only.ai_policy.share_metadata = false;
    let names_only = f.ws.save_connection(names_only).unwrap();
    let hidden = databrain_ai::ToolContext { profile: names_only, ..ctx("cloud", yes()) };
    let o = hidden.call("describe_table", &json!({"table": "orders"})).await.content;
    assert!(o.contains("metadata sharing disabled") && !o.contains("customer_id") && !o.contains("amount"), "{o}");
    let o = hidden.call("index_tables", &json!({"tables": ["main.orders"]})).await.content;
    assert!(o.starts_with("ERROR") && o.contains("metadata is disabled"), "{o}");
    f.ws.save_connection(profile.clone()).unwrap();
    assert!(cloud.call("describe_table", &json!({"table": "orders"})).await.content.contains("customer_id"), "shared again");

    // The grid's current result comes from a connection with AI off: it is
    // not summarised and not aggregated.
    let r = RunRequest {
        connection_id: closed.id.clone(),
        tab_id: uuid::Uuid::new_v4().to_string(),
        sql: "select name, email from customers".into(),
        base_offset: 0,
        row_limit: None,
        confirmed: true,
        origin: Origin::User,
        session_key: None,
        output_name: None,
        params: Default::default(),
        output_masks: None,
    };
    let rid = f.engine.run_and_wait(&f.hub, r, None).await.unwrap()[0].result.as_ref().unwrap().id.clone();
    let grid = ctx("cloud", Arc::new(Grid(rid.clone())));
    for (tool, args) in [("query_result", json!({"result_id": rid, "sql": "select count(*) from result"})), ("result_summary", json!({"result_id": rid}))] {
        let o = grid.call(tool, &args).await.content;
        assert!(o.starts_with("ERROR") && o.contains("AI is disabled"), "{tool}: {o}");
    }

    // Jobs: one whose step is on a connection the assistant may not work on
    // is not listed or read, and its steps no longer pass the review a run makes.
    let c = ctx("cloud", yes());
    let o = c
        .call("create_job", &json!({"name": "Count customers", "steps": [{"name": "customer_count", "action": "query", "connection": "Shop copy", "sql": "select count(*) n from customers"}]}))
        .await
        .content;
    assert!(o.contains("Created job"), "{o}");
    assert!(c.call("list_jobs", &json!({})).await.content.contains("Count customers"));
    let job = f.ws.get_job(&f.ws.list_jobs().unwrap()[0].id).unwrap();
    assert!(databrain_ai::jobs::review_step(&f.engine, &job.nodes[0]).is_ok());
    // A load that copies rows out of a connection that doesn't share its
    // rows (Shop copy) needs the user's own approval, with the reason; one
    // from a connection that shares them (Shop) is an ordinary approval.
    other("Scratch", AiPolicy { allow_write: true, ..Default::default() });
    let copy_job = |name: &str, step: &str, from: &str| {
        json!({"name": name, "steps": [
            {"name": step, "action": "query", "connection": from, "sql": "select name from customers"},
            {"name": format!("{step}_saved"), "action": "load", "after": [step], "target_connection": "Scratch", "target_table": "names"}
        ]})
    };
    for (name, step, from, asks_the_user) in [("Copy private", "private_names", "Shop copy", true), ("Copy shared", "shared_names", "Shop", false)] {
        let o = c.call("create_job", &copy_job(name, step, from)).await.content;
        assert!(o.contains("Created job"), "{o}");
        let id = f.ws.list_jobs().unwrap().into_iter().find(|j| j.name == name).unwrap().id;
        let reasons = databrain_ai::jobs::review(&f.engine, &f.ws.get_job(&id).unwrap(), &[]).unwrap();
        assert_eq!(reasons.iter().any(|r| r.contains("copies rows from \"Shop copy\" into \"Scratch\"")), asks_the_user, "{name}: {reasons:?}");
        f.ws.delete_job(&id).unwrap();
    }
    f.ws.save_connection(ConnectionProfile { ai_policy: AiPolicy { ai_enabled: false, ..Default::default() }, ..copy.clone() }).unwrap();
    assert_eq!(c.call("list_jobs", &json!({})).await.content, "No jobs.");
    let o = c.call("get_job", &json!({"job": "Count customers"})).await.content;
    assert!(o.starts_with("ERROR") && o.contains("no job") && !o.contains("select count"), "{o}");
    let e = databrain_ai::jobs::review_step(&f.engine, &job.nodes[0]).unwrap_err().to_string();
    assert!(e.contains("AI is disabled"), "{e}");
    // Shared with the assistant, not with MCP: an MCP client doesn't see it either.
    f.ws.save_connection(copy.clone()).unwrap();
    let mcp = databrain_ai::ToolContext { caller: databrain_ai::Caller::Mcp, provider_id: None, ..ctx("cloud", yes()) };
    assert_eq!(mcp.call("list_jobs", &json!({})).await.content, "No jobs.");

    // Notes: a long one is refused; one the assistant wrote is marked as its own when given back.
    let o = c.call("add_knowledge_note", &json!({"note": "x".repeat(601)})).await.content;
    assert!(o.starts_with("ERROR") && o.contains("601 characters"), "{o}");
}
