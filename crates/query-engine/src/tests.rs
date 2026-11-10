//! Engine tests on SQLite.
use super::*;
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::ConnectorKind;
use databrain_connector_sqlite::SqliteConnector;
use databrain_result_store::ViewSpec;

struct Fixture {
    engine: Arc<QueryEngine>,
    sink: Arc<CollectingSink>,
    ws: Arc<Workspace>,
    conn_id: String,
    _dir: TempFile,
}

struct TempFile(std::path::PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn fixture(env: EnvTag) -> Fixture {
    fixture_with_store(env, Arc::new(MemoryStore::default()))
}

fn fixture_with_store(env: EnvTag, secrets: Arc<dyn SecretStore>) -> Fixture {
    let path = std::env::temp_dir().join(format!("databrain-qe-{}-{}.db", std::process::id(), uuid::Uuid::new_v4()));
    let mut reg = ConnectorRegistry::new();
    reg.register(Arc::new(SqliteConnector));
    let ws = Arc::new(Workspace::open_in_memory().unwrap());
    let profile = ws
        .save_connection(ConnectionProfile {
            id: String::new(),
            name: "Local".into(),
            config: {
                let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
                c.file_path = Some(path.to_string_lossy().into());
                c
            },
            color: None,
            env,
            folder_id: None,
            has_secret: false,
            ai_policy: Default::default(),
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    let sink = Arc::new(CollectingSink::default());
    let engine = QueryEngine::new(reg, ws.clone(), secrets, Arc::new(ResultStore::new()), sink.clone());
    Fixture { engine, sink, ws, conn_id: profile.id, _dir: TempFile(path) }
}

fn req(f: &Fixture, sql: &str) -> RunRequest {
    RunRequest {
        connection_id: f.conn_id.clone(),
        tab_id: "tab1".into(),
        sql: sql.into(),
        base_offset: 0,
        row_limit: Some(100),
        confirmed: false,
        origin: Origin::User,
        session_key: None,
        output_name: None,
        params: Default::default(),
        output_masks: None,
    }
}

async fn wait_job(f: &Fixture, job_id: &str) -> Vec<JobEvent> {
    for _ in 0..500 {
        if !f.engine.is_running(job_id) {
            let evs = f.sink.0.lock().clone();
            if evs.iter().any(|e| matches!(e, JobEvent::JobFinished { job_id: j, .. } if j == job_id)) {
                return evs.into_iter().filter(|e| e.job_id() == job_id).collect();
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job did not finish");
}

fn started(r: RunResponse) -> String {
    match r {
        RunResponse::Started { job_id, .. } => job_id,
        other => panic!("expected start, got {other:?}"),
    }
}

#[tokio::test]
async fn parameters_are_bound_before_running() {
    let f = fixture(EnvTag::Dev);
    let sql = "select :d as d, :n as n, typeof(:n) as t, ':d' as lit";
    let e = f.engine.run(req(&f, sql)).unwrap_err();
    assert_eq!((e.kind.as_str(), e.code.as_deref()), ("invalid", Some("missing_parameters")));
    assert!(e.message.contains(":d") && e.message.contains(":n"), "{}", e.message);
    let mut r = req(&f, sql);
    r.params.insert("d".into(), databrain_connector_core::sql::ParamValue { value: "2026-09-09".into(), raw: false });
    r.params.insert("n".into(), databrain_connector_core::sql::ParamValue { value: "2000".into(), raw: false });
    let resp = f.engine.run(r).unwrap();
    let RunResponse::Started { job_id, statements } = resp else { panic!() };
    assert_eq!(statements[0].sql, "select '2026-09-09' as d, 2000 as n, typeof(2000) as t, ':d' as lit");
    assert_eq!((statements[0].start, statements[0].end), (0, sql.len()), "spans of the editor text");
    let evs = wait_job(&f, &job_id).await;
    let rid = evs
        .iter()
        .find_map(|e| match e {
            JobEvent::StatementFinished { result: Some(r), .. } => Some(r.id.clone()),
            _ => None,
        })
        .expect("result");
    let page = f.engine.results().get(&rid).unwrap().lock().page(&Default::default(), 0, 1).unwrap();
    let row = format!("{:?}", page.rows[0]);
    assert!(row.contains("2026-09-09") && row.contains("integer"), "{row}");
}

#[tokio::test]
async fn cells_share_session_but_keep_results() {
    let f = fixture(EnvTag::Dev);
    let cell = |tab: &str, sql: &str| RunRequest { tab_id: tab.into(), session_key: Some("nb:1".into()), ..req(&f, sql) };
    let j1 = started(f.engine.run(cell("nb:1:a", "create temp table tt as select 42 as v; select * from tt;")).unwrap());
    let e1 = wait_job(&f, &j1).await;
    let r1 = e1
        .iter()
        .find_map(|e| match e {
            JobEvent::StatementFinished { result: Some(r), .. } => Some(r.id.clone()),
            _ => None,
        })
        .unwrap();
    // Temp table (session state) is visible to another cell of the notebook.
    let j2 = started(f.engine.run(cell("nb:1:b", "select v from tt")).unwrap());
    let e2 = wait_job(&f, &j2).await;
    assert!(e2.iter().any(|e| matches!(e, JobEvent::StatementFinished { result: Some(r), .. } if r.total_rows == 1)), "{e2:?}");
    // Cell a's result survives cell b's run.
    assert!(f.engine.results().get(&r1).is_ok());
    // A different notebook/tab has its own session.
    let j3 = started(f.engine.run(req(&f, "select v from tt")).unwrap());
    let e3 = wait_job(&f, &j3).await;
    assert!(e3.iter().any(|e| matches!(e, JobEvent::StatementFailed { .. })));
    // Closing the notebook session releases it.
    f.engine.close_tab("nb:1");
    let j4 = started(f.engine.run(cell("nb:1:b", "select v from tt")).unwrap());
    assert!(wait_job(&f, &j4).await.iter().any(|e| matches!(e, JobEvent::StatementFailed { .. })));
}

#[tokio::test]
async fn runs_script_and_records_history() {
    let f = fixture(EnvTag::Dev);
    let job =
        started(f.engine.run(req(&f, "create table t(id integer, name text);\ninsert into t values (1,'a'),(2,'b');\nselect * from t order by id;")).unwrap());
    let evs = wait_job(&f, &job).await;
    let finished: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            JobEvent::StatementFinished { result, rows_affected, .. } => Some((result.clone(), *rows_affected)),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 3);
    assert_eq!(finished[1].1, Some(2));
    let info = finished[2].0.clone().unwrap();
    assert_eq!(info.total_rows, 2);
    assert!(!info.truncated);
    let rs = f.engine.results().get(&info.id).unwrap();
    let page = rs.lock().page(&ViewSpec::default(), 0, 10).unwrap();
    assert_eq!(page.rows[1][1].as_deref(), Some("b"));
    assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Success, .. })));
    let hist = f.ws.list_history(&Default::default()).unwrap();
    assert_eq!(hist.len(), 3);
}

#[tokio::test]
async fn row_limit_truncates() {
    let f = fixture(EnvTag::None);
    let mut r = req(&f, "with recursive c(x) as (select 1 union all select x+1 from c where x < 5000) select x from c");
    r.row_limit = Some(250);
    let job = started(f.engine.run(r).unwrap());
    let evs = wait_job(&f, &job).await;
    let info = evs
        .iter()
        .find_map(|e| match e {
            JobEvent::StatementFinished { result, .. } => result.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(info.total_rows, 250);
    assert!(info.truncated);

    // Exactly at the limit is not truncated.
    let mut r = req(&f, "select 1 union all select 2");
    r.row_limit = Some(2);
    let job = started(f.engine.run(r).unwrap());
    let evs = wait_job(&f, &job).await;
    let info = evs
        .iter()
        .find_map(|e| match e {
            JobEvent::StatementFinished { result, .. } => result.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(info.total_rows, 2);
    assert!(!info.truncated);
}

#[tokio::test]
async fn manual_commit_opens_a_transaction_until_commit_or_rollback() {
    let f = fixture(EnvTag::None);
    let run = |sql: &str| RunRequest { confirmed: true, ..req(&f, sql) };
    let job = started(f.engine.run(run("create table t (a int)")).unwrap());
    wait_job(&f, &job).await;
    f.engine.set_manual_commit("tab1", true);
    let job = started(f.engine.run(run("insert into t values (1)")).unwrap());
    let evs = wait_job(&f, &job).await;
    assert!(evs.iter().any(|e| matches!(e, JobEvent::JobFinished { in_transaction: true, .. })), "{evs:?}");
    assert!(f.engine.in_transaction(&f.conn_id, "tab1"));
    // Reads don't open one; Rollback undoes the insert.
    assert!(!f.engine.end_transaction(&f.conn_id, "tab1", false).await.unwrap());
    let count = |f: &Fixture| {
        let ws = f.engine.clone();
        let id = f.conn_id.clone();
        async move {
            let s = ws.session_for_tests(&id).await.unwrap();
            let c = s.execute("select * from t", ExecOptions::default()).await.unwrap().collect().await.unwrap();
            c.batches.iter().map(|b| b.num_rows()).sum::<usize>()
        }
    };
    assert_eq!(count(&f).await, 0);
    // Commit keeps it.
    let job = started(f.engine.run(run("insert into t values (2)")).unwrap());
    wait_job(&f, &job).await;
    assert!(!f.engine.end_transaction(&f.conn_id, "tab1", true).await.unwrap());
    assert_eq!(count(&f).await, 1);
    // Auto-commit again: no transaction is left open.
    f.engine.set_manual_commit("tab1", false);
    let job = started(f.engine.run(run("insert into t values (3)")).unwrap());
    wait_job(&f, &job).await;
    assert!(!f.engine.in_transaction(&f.conn_id, "tab1"));
}

/// A connection's own idle time and connect timeout win over the global
/// settings; `0` minutes keeps its sessions open.
#[tokio::test]
async fn per_connection_idle_time_and_connect_timeout() {
    let f = fixture(EnvTag::None);
    let set = |k: &str, v: Option<&str>| {
        let mut p = f.ws.get_connection(&f.conn_id).unwrap();
        match v {
            Some(v) => p.config.options.insert(k.into(), v.into()),
            None => p.config.options.remove(k),
        };
        f.ws.save_connection(p).unwrap();
    };
    let open = |f: &Fixture| {
        let (e, id) = (f.engine.clone(), f.conn_id.clone());
        async move { e.session_for_tests(&id).await.unwrap() }
    };
    drop(open(&f).await);
    set(databrain_connector_core::IDLE_DISCONNECT_OPTION, Some("0"));
    assert_eq!(f.engine.close_idle_with(Some(Duration::ZERO)), 0, "0 = never for this connection");
    set(databrain_connector_core::IDLE_DISCONNECT_OPTION, Some("30"));
    assert_eq!(f.engine.close_idle_with(Some(Duration::ZERO)), 0, "its own 30 minutes, not the global zero");
    set(databrain_connector_core::IDLE_DISCONNECT_OPTION, None);
    assert_eq!(f.engine.close_idle_with(None), 0, "global: never");
    assert_eq!(f.engine.close_idle_with(Some(Duration::ZERO)), 1, "global applies without its own");

    let mut cfg = f.ws.get_connection(&f.conn_id).unwrap().config;
    assert_eq!(f.engine.connect_timeout_for(&cfg), f.engine.connect_timeout());
    cfg.options.insert(databrain_connector_core::CONNECT_TIMEOUT_OPTION.into(), "90".into());
    assert_eq!(f.engine.connect_timeout_for(&cfg), Duration::from_secs(90));
    cfg.options.insert(databrain_connector_core::CONNECT_TIMEOUT_OPTION.into(), "1".into());
    assert_eq!(f.engine.connect_timeout_for(&cfg), Duration::from_secs(3), "kept within 3–600");
}

#[tokio::test]
async fn idle_sessions_close_but_not_mid_transaction() {
    let f = fixture(EnvTag::None);
    let job = started(f.engine.run(req(&f, "select 1")).unwrap());
    wait_job(&f, &job).await;
    assert!(f.engine.connected_ids().contains(&f.conn_id));
    assert_eq!(f.engine.close_idle(Duration::from_secs(3600)), 0, "recently used");
    assert_eq!(f.engine.close_idle(Duration::ZERO), 1);
    assert!(f.engine.connected_ids().is_empty());
    // An open transaction keeps the session (and its uncommitted rows).
    let job = started(f.engine.run(req(&f, "begin")).unwrap());
    wait_job(&f, &job).await;
    assert_eq!(f.engine.close_idle(Duration::ZERO), 0);
    let job = started(f.engine.run(req(&f, "rollback")).unwrap());
    wait_job(&f, &job).await;
    assert_eq!(f.engine.close_idle(Duration::ZERO), 1);
    // Setting: minutes, 0 = never.
    assert_eq!(f.engine.idle_disconnect(), Some(Duration::from_secs(IDLE_DISCONNECT_DEFAULT_MINUTES * 60)));
    f.ws.set_setting(IDLE_DISCONNECT_SETTING, &serde_json::json!(0)).unwrap();
    assert_eq!(f.engine.idle_disconnect(), None);
}

#[tokio::test]
async fn error_stops_script_with_offset() {
    let f = fixture(EnvTag::None);
    let mut r = req(&f, "select 1;\nselec 2;\nselect 3;");
    r.base_offset = 100;
    let job = started(f.engine.run(r).unwrap());
    let evs = wait_job(&f, &job).await;
    let failed = evs
        .iter()
        .find_map(|e| match e {
            JobEvent::StatementFailed { index, statement_start, error, .. } => Some((*index, *statement_start, error.kind.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(failed, (1, 110, "query".to_string()));
    assert!(!evs.iter().any(|e| matches!(e, JobEvent::StatementStarted { index: 2, .. })));
    assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Error, .. })));
}

#[tokio::test]
async fn cancel_long_query() {
    let f = fixture(EnvTag::None);
    let job = started(f.engine.run(req(&f, "with recursive c(x) as (select 1 union all select x+1 from c) select count(*) from c")).unwrap());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.engine.cancel(&job));
    let evs = wait_job(&f, &job).await;
    assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Cancelled, .. })));
    // Tab session still works afterwards.
    let job = started(f.engine.run(req(&f, "select 42")).unwrap());
    let evs = wait_job(&f, &job).await;
    assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Success, .. })));
}

/// Runs started at the same moment on one tab: exactly one starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_run_per_tab_even_when_started_at_once() {
    let f = fixture(EnvTag::None);
    let slow = "with recursive c(x) as (select 1 union all select x+1 from c where x < 3000000) select count(*) from c";
    for _ in 0..5 {
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (engine, r, b) = (f.engine.clone(), req(&f, slow), barrier.clone());
                tokio::task::spawn_blocking(move || {
                    b.wait();
                    engine.run(r)
                })
            })
            .collect();
        let mut ids = vec![];
        for t in tasks {
            if let Ok(RunResponse::Started { job_id, .. }) = t.await.unwrap() {
                ids.push(job_id);
            }
        }
        assert_eq!(ids.len(), 1, "{ids:?}");
        f.engine.cancel(&ids[0]);
        wait_job(&f, &ids[0]).await;
    }
}

#[tokio::test]
async fn safety_confirmation() {
    let f = fixture(EnvTag::Prod);
    let r = f.engine.run(req(&f, "select 1")).unwrap();
    assert!(matches!(r, RunResponse::Started { .. }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    match f.engine.run(req(&f, "create table x(a int); delete from x")).unwrap() {
        RunResponse::NeedsConfirmation { reasons, .. } => {
            assert_eq!(reasons.len(), 2, "{reasons:?}");
        }
        other => panic!("{other:?}"),
    }
    let mut r = req(&f, "create table x(a int); delete from x");
    r.confirmed = true;
    let job = started(f.engine.run(r).unwrap());
    let evs = wait_job(&f, &job).await;
    assert!(matches!(evs.last(), Some(JobEvent::JobFinished { status: RunStatus::Success, .. })));
}

/// Production asks before admin and server-wide statements too, not only
/// DML and DDL; reads, transaction control and harmless USE/SET don't ask.
#[test]
fn prod_confirmation_covers_admin_statements() {
    use ConnectorKind::*;
    let profile = |kind| {
        let mut c = ConnectionConfig::new(kind, AuthMethod::None);
        c.host = Some("h".into());
        ConnectionProfile {
            id: "c".into(),
            name: "Prod".into(),
            config: c,
            color: None,
            env: EnvTag::Prod,
            folder_id: None,
            has_secret: false,
            ai_policy: Default::default(),
            created_at: 0,
            updated_at: 0,
        }
    };
    let asks = |kind, sql: &str| {
        let p = profile(kind);
        let plan: Vec<PlannedStatement> = databrain_connector_core::sql::split_statements(sql, kind)
            .into_iter()
            .enumerate()
            .map(|(index, s)| PlannedStatement {
                index,
                classification: databrain_connector_core::sql::classify(&s.sql, kind),
                sql: s.sql,
                start: s.start,
                end: s.end,
            })
            .collect();
        !QueryEngine::safety_reasons(&p, &plan).is_empty()
    };
    for (kind, sql) in [
        (Mysql, "KILL 42"),
        (Mysql, "SHUTDOWN"),
        (Mysql, "FLUSH TABLES WITH READ LOCK"),
        (Mysql, "SET GLOBAL max_connections = 1"),
        (Mysql, "SET @@global.read_only = 0"),
        (Mssql, "RESTORE DATABASE d FROM DISK = 'x.bak'"),
        (Mssql, "SHUTDOWN WITH NOWAIT"),
        (Postgres, "VACUUM FULL orders"),
        (Postgres, "REFRESH MATERIALIZED VIEW mv"),
        (Postgres, "SET default_transaction_read_only = off"),
        (Postgres, "select 1; CHECKPOINT"),
        (Postgres, "delete from t where id = 1"),
        (Postgres, "frobnicate"),
    ] {
        assert!(asks(kind, sql), "{kind:?}: {sql} must ask on production");
    }
    for (kind, sql) in [
        (Postgres, "select * from t"),
        (Postgres, "begin; select 1; commit"),
        (Postgres, "set search_path = app"),
        (Mysql, "use shop"),
        (Mysql, "set time_zone = '+00:00'"),
    ] {
        assert!(!asks(kind, sql), "{kind:?}: {sql} should run without asking");
    }
}

/// Refresh signs in again: the metadata session and idle tabs close, a
/// tab mid-transaction (or with a running statement) keeps its session.
#[tokio::test]
async fn reconnect_closes_metadata_and_idle_tabs_only() {
    let f = fixture(EnvTag::None);
    f.engine.list_schemas(&f.conn_id).await.unwrap();
    let job = started(f.engine.run(req(&f, "select 1")).unwrap());
    wait_job(&f, &job).await;
    let keys = |f: &Fixture| {
        let mut v: Vec<String> = f.engine.sessions.lock().keys().map(|(_, t)| t.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(keys(&f), vec![META_TAB.to_string(), "tab1".to_string()]);
    assert_eq!(f.engine.reconnect(&f.conn_id), 2);
    assert!(f.engine.connected_ids().is_empty());
    // The next catalog call reconnects on its own.
    f.engine.list_schemas(&f.conn_id).await.unwrap();
    let job = started(f.engine.run(req(&f, "begin")).unwrap());
    wait_job(&f, &job).await;
    assert_eq!(f.engine.reconnect(&f.conn_id), 1, "only the metadata session");
    assert_eq!(keys(&f), vec!["tab1".to_string()]);
    assert!(f.engine.in_transaction(&f.conn_id, "tab1"));
    let job = started(f.engine.run(req(&f, "rollback")).unwrap());
    wait_job(&f, &job).await;
    assert_eq!(f.engine.reconnect("other-connection"), 0);
    assert_eq!(f.engine.reconnect(&f.conn_id), 1);
}

/// A credential store that blocks (the OS keychain waiting on a permission
/// prompt): connecting fails within the connect time limit with a message
/// that says so, and the runtime keeps serving meanwhile.
#[tokio::test]
async fn blocked_credential_store_times_out_with_a_message() {
    struct Slow;
    impl SecretStore for Slow {
        fn get(&self, _: &databrain_auth::SecretRef) -> std::result::Result<Option<SecretString>, databrain_auth::AuthError> {
            std::thread::sleep(Duration::from_secs(8));
            Ok(None)
        }
        fn set(&self, _: &databrain_auth::SecretRef, _: &SecretString) -> std::result::Result<(), databrain_auth::AuthError> {
            Ok(())
        }
        fn delete(&self, _: &databrain_auth::SecretRef) -> std::result::Result<(), databrain_auth::AuthError> {
            Ok(())
        }
    }
    let f = fixture_with_store(EnvTag::None, Arc::new(Slow));
    f.ws.set_setting("connect_timeout_secs", &serde_json::json!(3)).unwrap();
    let mut p = f.ws.get_connection(&f.conn_id).unwrap();
    p.config.auth = AuthMethod::Password { user: "u".into() };
    f.ws.save_connection(p).unwrap();
    let started = Instant::now();
    let other = tokio::spawn(async { 1 + 1 });
    let e = f.engine.session_for_tests(&f.conn_id).await.err().expect("times out");
    assert!(e.message.contains("credential store") && e.message.contains("keychain"), "{}", e.message);
    assert!(started.elapsed() < Duration::from_secs(7), "{:?}", started.elapsed());
    assert_eq!(other.await.unwrap(), 2, "the runtime was not blocked");
}

#[tokio::test]
async fn metadata_and_close_tab() {
    let f = fixture(EnvTag::None);
    let job = started(f.engine.run(req(&f, "create table users(id integer primary key); select * from users")).unwrap());
    wait_job(&f, &job).await;
    let schemas = f.engine.list_schemas(&f.conn_id).await.unwrap();
    assert_eq!(schemas[0].name, "main");
    let objs = f.engine.list_objects(&f.conn_id, "main").await.unwrap();
    assert_eq!(objs[0].name, "users");
    let d = f.engine.describe(&f.conn_id, "main", "users").await.unwrap();
    assert!(d.columns[0].is_primary_key);
    assert!(f.engine.connected_ids().contains(&f.conn_id));
    let before = f.engine.outputs().for_tab("tab1");
    assert_eq!(before[0].sql, "select * from users");
    f.engine.close_tab("tab1");
    assert!(f.engine.results().get(&before[0].result_id).is_err());
    f.engine.disconnect(&f.conn_id);
    assert!(f.engine.connected_ids().is_empty());
}

/// A read-only connection runs reads only, for every origin and even when
/// confirmed (jobs and the AI confirm their runs): writes, transactions
/// and session statements could make it writable.
#[tokio::test]
async fn read_only_connections_refuse_everything_but_reads() {
    let f = fixture(EnvTag::Dev);
    let id = started(f.engine.run(RunRequest { confirmed: true, ..req(&f, "create table t (a int); insert into t values (1)") }).unwrap());
    wait_job(&f, &id).await;
    let mut p = f.ws.get_connection(&f.conn_id).unwrap();
    p.config.read_only = true;
    f.ws.save_connection(p).unwrap();
    f.engine.disconnect(&f.conn_id);
    for origin in [Origin::User, Origin::Ai, Origin::Mcp, Origin::Job] {
        for sql in ["delete from t", "drop table t", "select 1; insert into t values (2)", "begin", "pragma query_only = off", "attach 'x.db' as x"] {
            let e = f.engine.run(RunRequest { confirmed: true, origin, tab_id: format!("ro-{}", origin.as_str()), ..req(&f, sql) }).unwrap_err();
            assert!(e.message.contains("read-only") && e.message.contains("was not run"), "{origin:?} {sql}: {}", e.message);
        }
    }
    let id = started(f.engine.run(req(&f, "select count(*) from t")).unwrap());
    let evs = wait_job(&f, &id).await;
    assert!(evs.iter().any(|e| matches!(e, JobEvent::StatementFinished { result: Some(r), .. } if r.total_rows == 1)), "{evs:?}");
}

/// A statement's stored result is kept only when it finished: an error, a
/// stream that ends without `Done` and an abandoned read all remove it.
#[tokio::test]
async fn unfinished_statement_results_are_removed_from_the_store() {
    use databrain_connector_core::arrow::array::{Int64Array, RecordBatch};
    use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
    use databrain_connector_core::{ExecSummary, QueryStream, StreamEvent};
    let f = fixture(EnvTag::Dev);
    let r = req(&f, "select 1");
    let profile = f.ws.get_connection(&f.conn_id).unwrap();
    let stmt = f.engine.plan(&profile, "select 1").remove(0);
    let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, false)]));
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1, 2]))]).unwrap();
    // `end`: how the stream ends after a schema and a batch.
    let run = |job: &'static str, end: Option<std::result::Result<StreamEvent, ConnectorError>>| {
        let (tx, stream) = QueryStream::channel(8);
        let (schema, batch) = (schema.clone(), batch.clone());
        tokio::spawn(async move {
            tx.send(StreamEvent::Schema(schema)).await;
            tx.send(StreamEvent::Batch(batch)).await;
            match end {
                Some(Ok(ev)) => {
                    tx.send(ev).await;
                }
                Some(Err(e)) => {
                    tx.send_err(e).await;
                }
                None => {} // dropped: the stream ends without Done
            }
        });
        let (engine, r, stmt) = (f.engine.clone(), r.clone(), stmt.clone());
        async move { engine.collect_stream(job, &r, &stmt, None, CancellationToken::new(), stream, Instant::now()).await }
    };

    let ok = run("done", Some(Ok(StreamEvent::Done(ExecSummary::default())))).await.unwrap();
    assert_eq!(ok.result.unwrap().total_rows, 2);
    assert!(f.engine.results().contains("done:0"), "a finished result stays");

    let e = run("early", None).await.err().unwrap();
    assert_eq!(e.kind, "internal");
    assert!(!f.engine.results().contains("early:0"), "a stream that ends without Done leaves nothing");

    assert!(run("failed", Some(Err(ConnectorError::internal("boom")))).await.is_err());
    assert!(!f.engine.results().contains("failed:0"), "an error leaves nothing");

    // Read stops half way (the future is dropped).
    let (tx, stream) = QueryStream::channel(8);
    tx.send(StreamEvent::Schema(schema.clone())).await;
    let engine = f.engine.clone();
    let (r2, s2) = (r.clone(), stmt.clone());
    let reading = tokio::spawn(async move { engine.collect_stream("dropped", &r2, &s2, None, CancellationToken::new(), stream, Instant::now()).await });
    for _ in 0..100 {
        if f.engine.results().contains("dropped:0") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(f.engine.results().contains("dropped:0"));
    reading.abort();
    let _ = reading.await;
    assert!(!f.engine.results().contains("dropped:0"), "an abandoned read leaves nothing");
    drop(tx);
}

/// A run whose tab closed while it ran (its statement finished before it
/// saw the cancel) doesn't leave an active output behind; a run of an open
/// tab keeps its output.
#[tokio::test]
async fn outputs_of_a_run_whose_tab_closed_are_freed() {
    let f = fixture(EnvTag::Dev);
    let profile = f.ws.get_connection(&f.conn_id).unwrap();
    let run = |tab: &str, close: bool| {
        let r = RunRequest { tab_id: tab.into(), ..req(&f, "select 1 as a") };
        let plan = f.engine.plan(&profile, &r.sql);
        let job = format!("job-{tab}");
        f.engine
            .jobs
            .lock()
            .insert(job.clone(), JobHandle { tab_id: tab.into(), session_key: tab.into(), cancel: CancellationToken::new(), tab_closed: false });
        if close {
            f.engine.close_tab(tab);
            let jobs = f.engine.jobs.lock();
            let j = &jobs[&job];
            assert!(j.tab_closed && j.cancel.is_cancelled());
        }
        // The statement runs as if the cancel came too late.
        f.engine.clone().run_job(job, r, plan, CancellationToken::new(), None)
    };

    run("open", false).await;
    let open = f.engine.outputs().for_tab("open");
    assert_eq!(open.len(), 1);
    assert!(open[0].active && f.engine.results().contains(&open[0].result_id));

    run("closed", true).await;
    assert!(f.engine.outputs().for_tab("closed").is_empty(), "no active output for a closed tab");
    let freed = f.engine.outputs().list().into_iter().find(|o| o.tab_id == "closed").unwrap();
    assert_eq!(freed.state, OutputState::Evicted);
    assert!(!freed.active && !f.engine.results().contains(&freed.result_id));
}
