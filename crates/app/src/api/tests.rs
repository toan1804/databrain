//! Tests of the backend API, without a webview.

use super::*;
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_export::ExportFormat;
use databrain_query_engine::{CollectingSink, JobEvent};
use databrain_result_store::{ColumnFilter, FilterOp, SortKey};
use databrain_workspace::EnvTag;
use std::time::Duration;

#[test]
fn parameter_spans_are_utf16_and_previews_typed() {
    // "é" and "😀" are 1 and 2 UTF-16 units (2 and 4 bytes).
    let text = "select 'é😀', :day";
    let p = sql_parameters(ConnectorKind::Postgres, text);
    assert_eq!((p[0].name.as_str(), p[0].from, p[0].to), ("day", 14, 18));
    let mut v = std::collections::HashMap::new();
    v.insert("day".to_string(), databrain_connector_core::sql::ParamValue { value: "2026-09-09".into(), raw: false });
    v.insert("n".to_string(), databrain_connector_core::sql::ParamValue { value: "2000".into(), raw: false });
    let out = preview_parameters(ConnectorKind::Postgres, v);
    assert_eq!(out["day"].as_deref(), Some("'2026-09-09'"));
    assert_eq!(out["n"].as_deref(), Some("2000"));
}

#[test]
fn utf16_offsets() {
    let t = "é😀a";
    assert_eq!(byte_to_utf16(t, 2), 1);
    assert_eq!(byte_to_utf16(t, 6), 3);
    assert_eq!(utf16_to_byte(t, 3), 6);
    assert_eq!(utf16_to_byte(t, 99), t.len());
    let s = statement_at_cursor(ConnectorKind::Sqlite, "select 'é';\nselect 2;", 13).unwrap();
    assert_eq!((s.start, s.end, s.sql.as_str()), (12, 20, "select 2"));
}

#[derive(Default)]
pub(crate) struct TestUi(pub parking_lot::Mutex<Vec<(String, serde_json::Value)>>);
impl crate::ai_api::UiBridge for TestUi {
    fn emit(&self, channel: &str, payload: serde_json::Value) {
        self.0.lock().push((channel.into(), payload));
    }
    fn open_url(&self, _: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

fn state() -> (AppState, Arc<CollectingSink>, tempfile::TempDir) {
    let sink = Arc::new(CollectingSink::default());
    let ws = Arc::new(Workspace::open_in_memory().unwrap());
    let st = AppState::new(ws, Arc::new(MemoryStore::default()), sink.clone(), Arc::new(TestUi::default()));
    (st, sink, tempfile::tempdir().unwrap())
}

async fn finished_results(sink: &CollectingSink, job: &str) -> Vec<ResultInfo> {
    for _ in 0..500 {
        let evs = sink.0.lock().clone();
        if evs
            .iter()
            .any(|e| matches!(e, JobEvent::JobFinished { job_id, .. } if job_id == job))
        {
            return evs
                .into_iter()
                .filter_map(|e| match e {
                    JobEvent::StatementFinished { job_id, result: Some(r), .. } if job_id == job => Some(r),
                    _ => None,
                })
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job did not finish")
}

/// End-to-end: create a SQLite connection, run a script, page/filter/sort/
/// find/stats/copy/export the result, save a query and read history.
#[tokio::test]
async fn end_to_end_sqlite() {
    let (st, sink, dir) = state();
    let db = dir.path().join("demo.db");
    let profile = save_connection(
        &st,
        SaveConnectionArgs {
            profile: ConnectionProfile {
                id: String::new(),
                name: "Demo".into(),
                config: ConnectionConfig {
                    kind: ConnectorKind::Sqlite,
                    host: None,
                    port: None,
                    database: None,
                    file_path: Some(db.to_string_lossy().into()),
                    auth: AuthMethod::None,
                    ssl_mode: Default::default(),
                    read_only: false,
                    options: Default::default(),
                    ssh: None,
                },
                color: None,
                env: EnvTag::Dev,
                folder_id: None,
                has_secret: false,
                ai_policy: Default::default(),
                created_at: 0,
                updated_at: 0,
            },
            secret: None,
            clear_secret: false,
            extra_secrets: Default::default(),
        },
    )
    .unwrap();

    let t = test_connection(&st, profile.config.clone(), None, None, None, None).await.unwrap();
    assert!(t.server_version.starts_with("SQLite"));

    let script = "create table people(id integer primary key, name text, city text, age integer);\n\
        insert into people(name, city, age) values ('Ann','Hanoi',31),('Binh','Hue',25),('Chi','Hanoi',42),('Dung',null,19);\n\
        select * from people;";
    let resp = run_query(
        &st,
        RunRequest {
            connection_id: profile.id.clone(),
            tab_id: "t1".into(),
            sql: script.into(),
            base_offset: 5,
            row_limit: Some(1000),
            confirmed: false,
            origin: Default::default(),
            session_key: None,
            output_name: None,
            params: Default::default(),
        },
    )
    .unwrap();
    let RunResponse::Started { job_id, statements } = resp else {
        panic!("expected start")
    };
    assert_eq!(statements.len(), 3);
    assert_eq!(statements[0].start, 5);
    let results = finished_results(&sink, &job_id).await;
    assert_eq!(results.len(), 1);
    let rid = results[0].id.clone();
    assert_eq!(results[0].total_rows, 4);

    let view = ViewSpec {
        filters: vec![ColumnFilter {
            column: 2,
            op: FilterOp::Equals,
            value: "Hanoi".into(),
        }],
        quick_filter: None,
        sort: vec![SortKey {
            column: 3,
            descending: true,
        }],
    };
    let page = fetch_page(&st, rid.clone(), view.clone(), 0, 100).await.unwrap();
    assert_eq!(page.view_rows, 2);
    assert_eq!(page.rows[0][1].as_deref(), Some("Chi"));

    let f = find_in_result(&st, rid.clone(), ViewSpec::default(), "han".into(), None, 100, false)
        .await
        .unwrap();
    assert_eq!(f.matches.len(), 2);

    let stats = column_stats(&st, rid.clone(), ViewSpec::default(), 2).await.unwrap();
    assert_eq!((stats.nulls, stats.distinct), (1, 2));

    let tsv = copy_rows(
        &st,
        rid.clone(),
        view.clone(),
        0,
        1,
        ExportOptions::new(ExportFormat::Tsv),
    )
    .await
    .unwrap();
    assert_eq!(tsv, "id\tname\tcity\tage\n3\tChi\tHanoi\t42\n");

    let out = dir.path().join("hanoi.csv");
    let n = export_result(
        &st,
        rid.clone(),
        view,
        ExportOptions::new(ExportFormat::Csv),
        out.to_string_lossy().into(),
    )
    .await
    .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "id,name,city,age\n3,Chi,Hanoi,42\n1,Ann,Hanoi,31\n"
    );
    assert!(export_result(&st, rid.clone(), ViewSpec::default(), ExportOptions::new(ExportFormat::Csv), "rel.csv".into())
        .await
        .is_err());

    // Explorer
    let objs = list_objects(&st, &profile.id, "main").await.unwrap();
    assert_eq!(objs[0].name, "people");
    assert!(list_connections(&st).unwrap()[0].connected);

    // Saved queries + history
    save_query(
        &st,
        SavedQuery {
            id: String::new(),
            name: "Hanoi people".into(),
            sql: "select * from people where city = 'Hanoi'".into(),
            connection_id: Some(profile.id.clone()),
            folder_id: None,
            description: None,
            tags: vec![],
            ai_example: false,
            created_at: 0,
            updated_at: 0,
        },
    )
    .unwrap();
    assert_eq!(list_saved_queries(&st, Some("hanoi".into())).unwrap().len(), 1);
    assert_eq!(list_history(&st, HistoryQuery::default()).unwrap().len(), 3);

    // Clearing history drops the results it produced, except what a tab still shows.
    let again = |sql: &str| RunRequest {
        connection_id: profile.id.clone(),
        tab_id: "t1".into(),
        sql: sql.into(),
        base_offset: 0,
        row_limit: Some(1000),
        confirmed: true,
        origin: Default::default(),
        session_key: None,
        output_name: None,
        params: Default::default(),
    };
    let RunResponse::Started { job_id, .. } = run_query(&st, again("select count(*) from people")).unwrap() else { panic!() };
    let latest = finished_results(&sink, &job_id).await[0].id.clone();
    assert!(result_info(&st, &rid).is_ok());
    assert_eq!(clear_history(&st, Some(databrain_workspace::HistoryClear::All)).unwrap(), 4);
    assert!(result_info(&st, &rid).is_err(), "the older result went with its history entry");
    assert!(result_info(&st, &latest).is_ok(), "the tab's current result stays");

    close_tab(&st, "t1");
    assert!(result_info(&st, &rid).is_err());
    delete_connection(&st, &profile.id).unwrap();
    assert!(list_connections(&st).unwrap().is_empty());
}

/// Switching to the local vault moves saved passwords; the connection
/// keeps working and a restart (new store instance) reads the vault.
#[tokio::test]
async fn credential_store_switch_moves_secrets() {
    use databrain_auth::{StoreKind, SwitchableStore, VaultStore};
    use secrecy::ExposeSecret;
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(Workspace::open_in_memory().unwrap());
    let keychain = Arc::new(MemoryStore::default());
    struct Shared(Arc<MemoryStore>);
    impl SecretStore for Shared {
        fn get(&self, r: &SecretRef) -> std::result::Result<Option<secrecy::SecretString>, databrain_auth::AuthError> { self.0.get(r) }
        fn set(&self, r: &SecretRef, v: &secrecy::SecretString) -> std::result::Result<(), databrain_auth::AuthError> { self.0.set(r, v) }
        fn delete(&self, r: &SecretRef) -> std::result::Result<(), databrain_auth::AuthError> { self.0.delete(r) }
    }
    let store = Arc::new(SwitchableStore::with_stores(StoreKind::Keychain, Box::new(Shared(keychain.clone())), Box::new(VaultStore::new(dir.path()))));
    let st = AppState::new(ws.clone(), store.clone(), Arc::new(CollectingSink::default()), Arc::new(TestUi::default()));
    st.set_credential_store(store.clone(), dir.path().to_path_buf());
    assert_eq!(credential_store(&st).kind, StoreKind::Keychain);
    let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, databrain_auth::AuthMethod::Password { user: "me".into() });
    cfg.host = Some("localhost".into());
    let saved = save_connection(
        &st,
        SaveConnectionArgs {
            profile: ConnectionProfile { id: String::new(), name: "PG".into(), config: cfg, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 },
            secret: Some("hunter2".into()),
            clear_secret: false,
            extra_secrets: [("ssh".to_string(), "sshpw".to_string())].into_iter().collect(),
        },
    )
    .unwrap();

    let rep = set_credential_store(&st, StoreKind::Vault).await.unwrap();
    assert_eq!((rep.moved, rep.failed.len()), (2, 0));
    assert_eq!(credential_store(&st).kind, StoreKind::Vault);
    assert_eq!(saved_store_kind(&ws), StoreKind::Vault);
    assert!(keychain.get(&SecretRef::for_connection(&saved.id)).unwrap().is_none(), "removed from the keychain");
    assert_eq!(st.secrets.get(&SecretRef::for_connection(&saved.id)).unwrap().unwrap().expose_secret(), "hunter2");
    // "Restart": a new vault store reads the same files.
    let again = SwitchableStore::with_stores(saved_store_kind(&ws), Box::new(MemoryStore::default()), Box::new(VaultStore::new(dir.path())));
    assert_eq!(again.get(&SecretRef::slot(&saved.id, "ssh")).unwrap().unwrap().expose_secret(), "sshpw");
}

#[test]
fn seeds_bundled_duckdb_extensions() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let plat = src.path().join("v1.5.6/osx_arm64");
    std::fs::create_dir_all(&plat).unwrap();
    std::fs::write(plat.join("excel.duckdb_extension"), b"bin").unwrap();
    std::fs::write(src.path().join("README.md"), b"x").unwrap();
    assert_eq!(seed_duckdb_extensions(src.path(), dst.path()), 1);
    assert!(dst.path().join("v1.5.6/osx_arm64/excel.duckdb_extension").is_file());
    assert!(!dst.path().join("README.md").exists());
    assert_eq!(seed_duckdb_extensions(src.path(), dst.path()), 0, "unchanged files are not copied again");
    #[cfg(feature = "duckdb")]
    assert_eq!(databrain_connector_duckdb::extension_dir().as_deref(), Some(&*dst.path().to_string_lossy()));
}

/// Deleting removes the connection, all its secrets and AI knowledge;
/// saved queries survive with no connection.
#[test]
fn delete_connection_cleans_up() {
    let (st, _sink, _dir) = state();
    let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: "me".into() });
    cfg.host = Some("db".into());
    let saved = save_connection(
        &st,
        SaveConnectionArgs {
            profile: ConnectionProfile { id: String::new(), name: "PG".into(), config: cfg, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 },
            secret: Some("pw".into()),
            clear_secret: false,
            extra_secrets: [("ssh".to_string(), "sshpw".to_string()), ("client_secret".to_string(), "cs".to_string())].into_iter().collect(),
        },
    )
    .unwrap();
    let id = saved.id.clone();
    st.secrets.set(&SecretRef::slot(&id, "oauth"), &"tokens".to_string().into()).unwrap();
    st.workspace
        .kn_save_note(databrain_workspace::KnNote { id: String::new(), connection_id: id.clone(), target: None, body: "rule".into(), author: "user".into(), status: databrain_workspace::NoteStatus::Approved, created_at: 0, replaces: None })
        .unwrap();
    let q = save_query(&st, SavedQuery { id: String::new(), name: "q".into(), sql: "select 1".into(), connection_id: Some(id.clone()), folder_id: None, description: None, tags: vec![], ai_example: false, created_at: 0, updated_at: 0 }).unwrap();

    delete_connection(&st, &id).unwrap();
    assert!(list_connections(&st).unwrap().is_empty());
    for r in [SecretRef::for_connection(&id), SecretRef::slot(&id, "ssh"), SecretRef::slot(&id, "client_secret"), SecretRef::slot(&id, "oauth")] {
        assert!(st.secrets.get(&r).unwrap().is_none(), "{r:?} left behind");
    }
    assert!(st.workspace.kn_notes(&id).unwrap().is_empty());
    let kept = list_saved_queries(&st, None).unwrap();
    assert_eq!(kept.iter().find(|x| x.id == q.id).unwrap().connection_id, None);
    assert!(delete_connection(&st, &id).is_err(), "unknown id");
}

#[test]
fn secrets_are_stored_in_secret_store_only() {
    let (st, _sink, _dir) = state();
    let saved = save_connection(
        &st,
        SaveConnectionArgs {
            profile: ConnectionProfile {
                id: String::new(),
                name: "PG".into(),
                config: ConnectionConfig {
                    kind: ConnectorKind::Postgres,
                    host: Some("localhost".into()),
                    port: Some(5432),
                    database: Some("postgres".into()),
                    file_path: None,
                    auth: AuthMethod::Password { user: "me".into() },
                    ssl_mode: Default::default(),
                    read_only: false,
                    options: Default::default(),
                    ssh: None,
                },
                color: None,
                env: EnvTag::None,
                folder_id: None,
                has_secret: false,
                ai_policy: Default::default(),
                created_at: 0,
                updated_at: 0,
            },
            secret: Some("hunter2".into()),
            clear_secret: false,
            extra_secrets: [("ssh".to_string(), "sshpw".to_string()), ("bogus".to_string(), "x".to_string())].into_iter().collect(),
        },
    )
    .unwrap();
    assert!(saved.has_secret);
    let json = serde_json::to_string(&st.workspace.get_connection(&saved.id).unwrap()).unwrap();
    assert!(!json.contains("hunter2"));
    let r = SecretRef::for_connection(&saved.id);
    use secrecy::ExposeSecret;
    assert_eq!(st.secrets.get(&r).unwrap().unwrap().expose_secret(), "hunter2");
    assert_eq!(st.secrets.get(&SecretRef::slot(&saved.id, "ssh")).unwrap().unwrap().expose_secret(), "sshpw");
    // Folder move
    let f = save_folder(&st, Folder { id: String::new(), parent_id: None, name: "Prod".into(), kind: FolderKind::Connections }).unwrap();
    move_to_folder(&st, FolderKind::Connections, &saved.id, Some(f.id.clone())).unwrap();
    assert_eq!(st.workspace.get_connection(&saved.id).unwrap().folder_id, Some(f.id));
    #[cfg(feature = "duckdb-engine")]
    {
        assert_eq!(file_scan_sql("/d/x.csv", false).unwrap(), "SELECT *\nFROM read_csv('/d/x.csv')\nLIMIT 1000;");
        assert!(file_scan_sql("/d/x.exe", false).is_err());
    }

    // Editing without a secret keeps it; clearing removes it.
    let mut p = saved.clone();
    p.has_secret = false;
    let kept = save_connection(&st, SaveConnectionArgs { profile: p.clone(), secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap();
    assert!(kept.has_secret);
    let cleared = save_connection(&st, SaveConnectionArgs { profile: p, secret: None, clear_secret: true, extra_secrets: [("ssh".to_string(), String::new())].into_iter().collect() }).unwrap();
    assert!(!cleared.has_secret);
    assert!(st.secrets.get(&r).unwrap().is_none());
    assert!(st.secrets.get(&SecretRef::slot(&saved.id, "ssh")).unwrap().is_none());
    assert!(st.secrets.get(&SecretRef::slot(&saved.id, "bogus")).unwrap().is_none());
}

/// A server that accepts TCP but never answers (what makes drivers hang):
/// Test gives up after the connect timeout, and Cancel stops it at once.
/// A closed port fails right away.
#[tokio::test(flavor = "multi_thread")]
async fn test_connection_times_out_and_cancels() {
    let (st, _sink, _dir) = state();
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = silent.local_addr().unwrap().port();
    let hold = tokio::spawn(async move {
        let mut keep = vec![];
        while let Ok((c, _)) = silent.accept().await {
            keep.push(c);
        }
    });
    let cfg = ConnectionConfig {
        kind: ConnectorKind::Postgres,
        host: Some("127.0.0.1".into()),
        port: Some(port),
        database: Some("x".into()),
        file_path: None,
        auth: AuthMethod::Password { user: "u".into() },
        ssl_mode: databrain_connector_core::SslMode::Disable,
        read_only: false,
        options: Default::default(),
        ssh: None,
    };
    st.engine.workspace().set_setting("connect_timeout_secs", &serde_json::json!(3)).unwrap();
    let t = std::time::Instant::now();
    let e = test_connection(&st, cfg.clone(), Some("p".into()), None, None, Some("a".into())).await.unwrap_err();
    assert!(e.message.contains("No connection after 3 s"), "{}", e.message);
    assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());

    st.engine.workspace().set_setting("connect_timeout_secs", &serde_json::json!(60)).unwrap();
    let st = Arc::new(st);
    let st2 = st.clone();
    let c2 = cfg.clone();
    let run = tokio::spawn(async move { test_connection(&st2, c2, Some("p".into()), None, None, Some("b".into())).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    assert!(cancel_test_connection(&st, "b"));
    let e = run.await.unwrap().unwrap_err();
    assert_eq!(e.kind, "cancelled");
    assert!(t.elapsed() < Duration::from_secs(1));
    assert!(!cancel_test_connection(&st, "b"), "finished tests are forgotten");
    hold.abort();

    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let t = std::time::Instant::now();
    let e = test_connection(&st, ConnectionConfig { port: Some(closed), ..cfg }, Some("p".into()), None, None, None).await.unwrap_err();
    assert!(e.message.contains("refused"), "{}", e.message);
    // Well before the 60 s timeout. Windows retries a refused connect
    // (about 2 s in all), other systems fail at once.
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

/// Live Postgres (DATABRAIN_PG_*): a good Test connects, a wrong password
/// fails at once, and a host that drops packets fails within the probe time.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test(flavor = "multi_thread")]
async fn test_connection_live_postgres() {
    let host = std::env::var("DATABRAIN_PG_HOST").expect("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    let (st, _sink, _dir) = state();
    let cfg = ConnectionConfig {
        kind: ConnectorKind::Postgres,
        host: Some(host),
        port: std::env::var("DATABRAIN_PG_PORT").ok().and_then(|p| p.parse().ok()),
        database: Some("postgres".into()),
        file_path: None,
        auth: AuthMethod::Password { user: std::env::var("DATABRAIN_PG_USER").unwrap_or_else(|_| "postgres".into()) },
        ssl_mode: databrain_connector_core::SslMode::Disable,
        read_only: false,
        options: Default::default(),
        ssh: None,
    };
    let pw = std::env::var("DATABRAIN_PG_PASSWORD").ok();
    let ok = test_connection(&st, cfg.clone(), pw, None, None, None).await.unwrap();
    assert!(ok.server_version.contains("PostgreSQL"), "{}", ok.server_version);
    let t = std::time::Instant::now();
    let e = test_connection(&st, cfg.clone(), Some("wrong".into()), None, None, None).await.unwrap_err();
    assert!(e.message.contains("password"), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(3));
    // TEST-NET-1 (RFC 5737): never routed, packets are dropped.
    let t = std::time::Instant::now();
    let e = test_connection(&st, ConnectionConfig { host: Some("192.0.2.1".into()), ..cfg }, Some("x".into()), None, None, None).await.unwrap_err();
    assert!(e.message.contains("No answer") || e.message.contains("Cannot reach"), "{}", e.message);
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

fn save_profile(st: &AppState, name: &str, config: ConnectionConfig, secret: Option<&str>) -> ConnectionProfile {
    save_connection(
        st,
        SaveConnectionArgs {
            profile: ConnectionProfile {
                id: String::new(),
                name: name.into(),
                config,
                color: None,
                env: EnvTag::Dev,
                folder_id: None,
                has_secret: false,
                ai_policy: Default::default(),
                created_at: 0,
                updated_at: 0,
            },
            secret: secret.map(str::to_string),
            clear_secret: false,
            extra_secrets: Default::default(),
        },
    )
    .unwrap()
}

async fn exec(st: &AppState, id: &str, sql: &str) {
    let s = st.engine.session_for_tests(id).await.unwrap();
    for stmt in sql.split(';').map(str::trim).filter(|x| !x.is_empty()) {
        s.execute(stmt, Default::default()).await.unwrap().collect().await.unwrap();
    }
}

/// SQLite (fingerprints, no object versions): the explorer is served from
/// the cache without a connection, unchanged schemas are not listed again,
/// a changed one is listed again and its columns go stale.
#[tokio::test]
async fn explorer_cache_sqlite() {
    let (st, _sink, dir) = state();
    let db = dir.path().join("x.db");
    let cfg = ConnectionConfig {
        kind: ConnectorKind::Sqlite,
        host: None,
        port: None,
        database: None,
        file_path: Some(db.to_string_lossy().into()),
        auth: AuthMethod::None,
        ssl_mode: Default::default(),
        read_only: false,
        options: Default::default(),
        ssh: None,
    };
    let p = save_profile(&st, "x", cfg.clone(), None);
    assert!(cached_explorer(&st, &p.id).unwrap().schemas.is_none(), "nothing cached yet");
    exec(&st, &p.id, "create table a (id int); create table b (id int)").await;
    let schemas = list_schemas(&st, &p.id).await.unwrap();
    let main = schemas[0].name.clone();
    list_objects(&st, &p.id, &main).await.unwrap();
    describe(&st, &p.id, &main, "a").await.unwrap();
    flush_explorer_cache(&st).await;

    // Restart: a new engine over the same workspace, nothing connected.
    st.engine.disconnect(&p.id);
    let c = cached_explorer(&st, &p.id).unwrap();
    assert_eq!(c.schemas.unwrap().len(), schemas.len());
    assert_eq!(cached_objects(&st, &p.id, &main).unwrap().unwrap().objects.len(), 2);
    assert!(!cached_columns(&st, &p.id, &main, "a").unwrap().unwrap().stale);
    assert!(st.engine.connected_ids().is_empty(), "cache reads never connect");

    let r = revalidate_explorer(&st, &p.id, &[main.clone()], true).await.unwrap();
    assert!(r.fingerprints);
    // First check records the fingerprint (one listing), then nothing.
    let r = revalidate_explorer(&st, &p.id, &[main.clone()], true).await.unwrap();
    assert_eq!((r.refreshed.len(), r.unchanged), (0, 1), "{r:?}");
    assert!(revalidate_explorer(&st, &p.id, &[], false).await.unwrap().skipped, "checked moments ago");

    exec(&st, &p.id, "create table c (id int)").await;
    let r = revalidate_explorer(&st, &p.id, &[], true).await.unwrap();
    assert_eq!(r.refreshed.len(), 1);
    assert_eq!(r.refreshed[0].mode, RefreshMode::Full);
    assert_eq!(cached_objects(&st, &p.id, &main).unwrap().unwrap().objects.len(), 3);
    assert!(cached_columns(&st, &p.id, &main, "a").unwrap().unwrap().stale, "no versions: columns are checked again");

    // Pointing the connection elsewhere clears the cache.
    let mut moved = p.clone();
    moved.config.file_path = Some(dir.path().join("y.db").to_string_lossy().into());
    save_profile_update(&st, moved);
    assert!(cached_explorer(&st, &p.id).unwrap().schemas.is_none());
}

fn save_profile_update(st: &AppState, p: ConnectionProfile) {
    save_connection(st, SaveConnectionArgs { profile: p, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap();
}

/// Live Postgres (object versions): after a change only the changed and
/// new tables are fetched; untouched tables keep fresh columns.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test(flavor = "multi_thread")]
async fn explorer_incremental_live_postgres() {
    let host = std::env::var("DATABRAIN_PG_HOST").expect("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    let (st, _sink, _dir) = state();
    let cfg = ConnectionConfig {
        kind: ConnectorKind::Postgres,
        host: Some(host),
        port: std::env::var("DATABRAIN_PG_PORT").ok().and_then(|p| p.parse().ok()),
        database: Some("postgres".into()),
        file_path: None,
        auth: AuthMethod::Password { user: std::env::var("DATABRAIN_PG_USER").unwrap_or_else(|_| "postgres".into()) },
        ssl_mode: databrain_connector_core::SslMode::Disable,
        read_only: false,
        options: Default::default(),
        ssh: None,
    };
    let p = save_profile(&st, "pg", cfg, std::env::var("DATABRAIN_PG_PASSWORD").ok().as_deref());
    exec(&st, &p.id, "drop schema if exists inc_a cascade; drop schema if exists inc_b cascade").await;
    let mut ddl = String::from("create schema inc_a; create schema inc_b; create table inc_b.z (x int);");
    for i in 0..50 {
        ddl.push_str(&format!("create table inc_a.t{i} (id int primary key);"));
    }
    exec(&st, &p.id, &ddl).await;
    assert!(list_schemas(&st, &p.id).await.unwrap().iter().any(|s| s.name == "inc_a"));
    assert_eq!(list_objects(&st, &p.id, "inc_a").await.unwrap().len(), 50);
    list_objects(&st, &p.id, "inc_b").await.unwrap();
    describe(&st, &p.id, "inc_a", "t1").await.unwrap();
    describe(&st, &p.id, "inc_a", "t2").await.unwrap();

    let r = refresh_schema(&st, &p.id, "inc_a").await.unwrap();
    assert_eq!(r.mode, RefreshMode::Unchanged);
    revalidate_explorer(&st, &p.id, &[], true).await.unwrap();

    exec(&st, &p.id, "alter table inc_a.t1 add column note text; drop table inc_a.t3; create table inc_a.t_new (y int); create function inc_a.f() returns int language sql as 'select 1'").await;
    let r = revalidate_explorer(&st, &p.id, &["inc_a".into()], true).await.unwrap();
    let a = r.refreshed.iter().find(|x| x.schema == "inc_a").expect("inc_a changed");
    assert_eq!((a.mode.clone(), a.added, a.changed, a.removed), (RefreshMode::Incremental, 2, 1, 1), "{a:?}");
    assert!(r.refreshed.iter().all(|x| x.schema != "inc_b"), "unchanged schema not read: {r:?}");
    let names: Vec<String> = cached_objects(&st, &p.id, "inc_a").unwrap().unwrap().objects.iter().map(|o| o.name.clone()).collect();
    assert!(names.contains(&"t_new".to_string()) && names.contains(&"f".to_string()) && !names.contains(&"t3".to_string()));
    assert_eq!(names.len(), 51);
    assert!(cached_columns(&st, &p.id, "inc_a", "t1").unwrap().unwrap().stale);
    assert!(!cached_columns(&st, &p.id, "inc_a", "t2").unwrap().unwrap().stale, "untouched table keeps fresh columns");
    assert_eq!(refresh_schema(&st, &p.id, "inc_a").await.unwrap().mode, RefreshMode::Unchanged);
    exec(&st, &p.id, "drop schema inc_a cascade; drop schema inc_b cascade").await;
}

/// Lineage on SQLite: columns of uncached tables are described for `*`,
/// every query is checked with EXPLAIN (bad column = error, nothing runs),
/// parameters don't break parsing, offsets are UTF-16.
#[tokio::test]
async fn lineage_describes_and_checks_sqlite() {
    let (st, _sink, dir) = state();
    let cfg = ConnectionConfig {
        kind: ConnectorKind::Sqlite,
        host: None,
        port: None,
        database: None,
        file_path: Some(dir.path().join("l.db").to_string_lossy().into()),
        auth: AuthMethod::None,
        ssl_mode: Default::default(),
        read_only: false,
        options: Default::default(),
        ssh: None,
    };
    let p = save_profile(&st, "l", cfg, None);
    exec(&st, &p.id, "create table orders (id int, amount real, d text); insert into orders values (1, 2.5, '2026-09-09')").await;
    let sql = "-- é😀\nselect * from orders where d = :day;\nselect nope from orders;\ndelete from orders";
    let v = lineage(&st, Some(&p.id), ConnectorKind::Sqlite, sql, Default::default(), true).await.unwrap();
    assert_eq!(v.described, vec!["main.orders"], "uncached table described");
    let res = v.graph.nodes.iter().find(|n| n.name == "Result 1").unwrap();
    assert_eq!(res.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "amount", "d"]);
    let st_: Vec<&str> = v.checks.iter().map(|c| c.status.as_str()).collect();
    assert_eq!(st_, vec!["ok", "error", "skipped"], "{:?}", v.checks);
    assert!(v.checks[1].message.as_deref().unwrap_or("").contains("nope"));
    // The DELETE was only checked as "not a query": the row is still there.
    let s = st.engine.session_for_tests(&p.id).await.unwrap();
    let rows = databrain_connector_core::query_text(&*s, "select count(*) from orders").await.unwrap();
    assert_eq!(rows[0][0].as_deref(), Some("1"));
    // UTF-16 range of statement 1 (after the comment line with 1 + 2 units).
    let r = v.graph.statements[0].range;
    let units: Vec<u16> = sql.encode_utf16().collect();
    assert_eq!(String::from_utf16(&units[r.start..r.end]).unwrap(), "-- é😀\nselect * from orders where d = :day");
    // Offline / no connection: lineage still works, nothing checked.
    let v = lineage(&st, None, ConnectorKind::Sqlite, "select a from t", Default::default(), true).await.unwrap();
    assert_eq!(v.checks[0].status, "skipped");
    assert_eq!(v.graph.edges.len(), 1);
}

#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test(flavor = "multi_thread")]
async fn lineage_check_live_postgres_position() {
    let host = std::env::var("DATABRAIN_PG_HOST").expect("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    let (st, _sink, _dir) = state();
    let cfg = ConnectionConfig {
        kind: ConnectorKind::Postgres,
        host: Some(host),
        port: std::env::var("DATABRAIN_PG_PORT").ok().and_then(|p| p.parse().ok()),
        database: Some("postgres".into()),
        file_path: None,
        auth: AuthMethod::Password { user: std::env::var("DATABRAIN_PG_USER").unwrap_or_else(|_| "postgres".into()) },
        ssl_mode: databrain_connector_core::SslMode::Disable,
        read_only: false,
        options: Default::default(),
        ssh: None,
    };
    let p = save_profile(&st, "pg", cfg, std::env::var("DATABRAIN_PG_PASSWORD").ok().as_deref());
    exec(&st, &p.id, "drop table if exists lin_t; create table lin_t (id int, amount numeric)").await;
    let sql = "select 1;\nselect id, amout from lin_t";
    let v = lineage(&st, Some(&p.id), ConnectorKind::Postgres, sql, Default::default(), true).await.unwrap();
    assert_eq!(v.checks[0].status, "ok");
    assert_eq!(v.checks[1].status, "error");
    let pos = v.checks[1].position.expect("postgres gives a position");
    assert_eq!(&sql[pos..pos + 5], "amout", "{:?}", v.checks[1]);
    exec(&st, &p.id, "drop table lin_t").await;
}

/// A run the AI assistant starts runs the steps' own SQL in the DuckDB
/// sandbox, as its queries do; DataBrain's export (`COPY … TO`) still
/// writes its file. The same job run by hand reads files.
#[tokio::test]
async fn ai_job_runs_are_sandboxed() {
    let (st, _sink, dir) = state();
    let csv = dir.path().join("secret.csv");
    std::fs::write(&csv, "a\n1\n").unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let job: databrain_workspace::Job = serde_json::from_value(serde_json::json!({
        "name": "Files",
        "nodes": [
            {"id": "s1", "name": "nums", "kind": "query", "sql": "select range as n from range(3)"},
            {"id": "s2", "name": "nums_file", "kind": "export", "export_format": "csv", "export_folder": out.to_string_lossy(), "export_file": "nums"},
            {"id": "s3", "name": "secret", "kind": "query", "sql": format!("select * from read_csv('{}')", csv.display())}
        ],
        "edges": [{"from": "s1", "to": "s2"}]
    }))
    .unwrap();
    let job = save_job(&st, job).unwrap();
    let ctx = st.job_ctx();
    let run = |trigger: &'static str| {
        let (ctx, id) = (ctx.clone(), job.id.clone());
        async move {
            let started = crate::jobs::start(&ctx, &id, trigger, None).unwrap();
            for _ in 0..1000 {
                let r = ctx.workspace.job_runs(&id, 5).unwrap().into_iter().find(|r| r.id == started.id).unwrap();
                if r.status != "running" && !ctx.jobs.is_running(&id) {
                    return r;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("run did not finish")
        }
    };
    let step = |r: &databrain_workspace::JobRun, name: &str| r.nodes.iter().find(|n| n.name == name).unwrap().summary.clone();

    let by_ai = run("ai").await;
    assert_eq!(step(&by_ai, "nums").status, "success", "{by_ai:?}");
    assert_eq!(step(&by_ai, "nums_file").status, "success", "DataBrain's own COPY still writes the export: {by_ai:?}");
    let secret = step(&by_ai, "secret");
    assert_eq!(secret.status, "error");
    assert!(secret.error.as_deref().is_some_and(|e| e.contains("sandbox")), "{secret:?}");
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);

    let by_hand = run("manual").await;
    assert_eq!(by_hand.status, "success", "{by_hand:?}");
    assert_eq!(step(&by_hand, "secret").rows, Some(1));
}

/// The saved Instant Client folder is checked at launch like the thin
/// driver's helper: a folder saved before manifests were kept is hashed
/// once, then its libraries must stay the same to be loaded.
#[cfg(feature = "oracle")]
#[tokio::test]
async fn oracle_client_folder_is_checked_at_launch() {
    use databrain_connector_oracle::client;
    let (st, _sink, dir) = state();
    let ic = dir.path().join("instantclient_23_26");
    std::fs::create_dir_all(&ic).unwrap();
    std::fs::write(ic.join(client::library_name()), b"oci").unwrap();
    let d = ic.to_string_lossy().into_owned();
    st.workspace.set_setting(ORACLE_CLIENT_SETTING, &serde_json::json!(d)).unwrap();

    apply_oracle_client_setting(&st);
    let mut saved = None;
    for _ in 0..500 {
        saved = st.workspace.get_setting(ORACLE_CLIENT_MANIFEST_SETTING).unwrap();
        if saved.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let saved = saved.expect("hashed in the background");
    assert_eq!(saved["dir"], d.as_str());
    assert_eq!(saved["files"][client::library_name()].as_str().map(str::len), Some(64));
    assert_eq!(client::default_dir().as_deref(), Some(d.as_str()));

    // Next launch: the saved manifest is used; a changed library is refused.
    std::fs::write(ic.join(client::library_name()), b"replaced").unwrap();
    apply_oracle_client_setting(&st);
    match client::init_client(None, &[]) {
        Err(client::InitError::Changed(e)) => assert!(e.contains(client::library_name()), "{e}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(st.workspace.get_setting(ORACLE_CLIENT_MANIFEST_SETTING).unwrap(), Some(saved), "not re-hashed over");
    client::set_trusted(None);
    client::set_default_dir(None);
}
