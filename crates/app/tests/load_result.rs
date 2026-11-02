//! "Load into…" from the grid: a result's rows go into a table of another
//! connection with the job load code.

use std::sync::Arc;

use databrain_app::api::{self, AppState, LoadResultArgs};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::{EventSink, JobEvent, RunRequest};
use databrain_workspace::{ConnectionProfile, EnvTag, LoadMode, Workspace};

struct Quiet;
impl EventSink for Quiet {
    fn emit(&self, _: JobEvent) {}
}
struct NoUi;
impl databrain_app::ai_api::UiBridge for NoUi {
    fn emit(&self, _: &str, _: serde_json::Value) {}
    fn open_url(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

async fn run(st: &AppState, conn: &str, sql: &str) -> databrain_query_engine::StatementOutcomeView {
    let out = st
        .engine
        .run_and_wait(
            &st.hub,
            RunRequest {
                connection_id: conn.into(),
                tab_id: "t".into(),
                sql: sql.into(),
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin: Default::default(),
                session_key: None,
                output_name: None,
                params: Default::default(),
            },
            None,
        )
        .await
        .unwrap();
    out.into_iter().next_back().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn loads_a_result_into_another_connection() {
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let duck = st.engine.results_connection().unwrap().id;
    let dir = tempfile::tempdir().unwrap();
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(dir.path().join("target.db").to_string_lossy().into_owned());
    let profile = ConnectionProfile {
        id: String::new(),
        name: "target".into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    let target =
        api::save_connection(&st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;

    let r = run(&st, &duck, "select i as id, 'name ' || i as name from range(25) t(i)").await;
    let info = r.result.unwrap();
    let args = |mode: LoadMode| LoadResultArgs {
        target_connection_id: target.clone(),
        table: "people".into(),
        mode,
        key_columns: vec!["id".into()],
        create_table: true,
    };

    // Dry run keeps nothing.
    let dry = api::dry_run_result_load(&st, &info.id, args(LoadMode::Append)).await.unwrap();
    assert!(dry.ok, "{dry:?}");
    assert!(run(&st, &target, "select count(*) from people").await.error.is_some(), "the table was not created");

    let rep = api::load_result(&st, &info.id, args(LoadMode::Append), "l1").await.unwrap();
    assert_eq!(rep.rows, 25);
    assert!(rep.created_table);
    let n = run(&st, &target, "select count(*) as n, max(name) as m from people").await.result.unwrap();
    assert_eq!(n.total_rows, 1);

    // Merge by key: same 25 rows again, no duplicates.
    let rep = api::load_result(&st, &info.id, args(LoadMode::Merge), "l2").await.unwrap();
    assert!(rep.rows > 0);
    let page = api::fetch_page(&st, run(&st, &target, "select count(*) from people").await.result.unwrap().id, Default::default(), 0, 1).await.unwrap();
    assert_eq!(page.rows[0][0].as_deref(), Some("25"));

    // Read-only targets are refused.
    let mut p = st.workspace.get_connection(&target).unwrap();
    p.config.read_only = true;
    st.workspace.save_connection(p).unwrap();
    let e = api::load_result(&st, &info.id, args(LoadMode::Append), "l3").await.unwrap_err();
    assert!(e.message.contains("read-only"), "{}", e.message);
}

/// The target path is checked part by part; a missing schema (or DuckDB
/// database) stops the load before anything is created, with what to run.
#[tokio::test(flavor = "multi_thread")]
async fn checks_the_target_path_before_loading() {
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let duck = st.engine.results_connection().unwrap().id;
    let dir = tempfile::tempdir().unwrap();
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(dir.path().join("t.db").to_string_lossy().into_owned());
    let profile = ConnectionProfile {
        id: String::new(),
        name: "lite".into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    let lite = api::save_connection(&st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;
    run(&st, &lite, "create table people (id integer primary key, name text)").await;
    run(&st, &lite, "create view adults as select * from people").await;

    // Two levels (SQLite: attached database.table).
    let t = api::check_load_target(&st, &lite, "people").await.unwrap();
    assert_eq!((t.resolved.as_str(), t.schema_exists, t.table_exists), ("main.people", Some(true), Some(true)));
    assert!(t.problems.is_empty() && t.notes.iter().any(|n| n.contains("main is used")), "{t:?}");
    let t = api::check_load_target(&st, &lite, "main.newtable").await.unwrap();
    assert_eq!((t.table_exists, t.problems.len()), (Some(false), 0), "a missing table is fine: it can be created");
    let t = api::check_load_target(&st, &lite, "nosuch.t").await.unwrap();
    assert_eq!(t.schema_exists, Some(false));
    assert!(t.problems[0].contains("nosuch doesn't exist in lite"), "{t:?}");
    assert_eq!(t.create_sql.as_deref(), Some(r#"ATTACH DATABASE 'file.db' AS "nosuch""#));
    // Another case is accepted: the database's spelling is used.
    let t = api::check_load_target(&st, &lite, "MAIN.People").await.unwrap();
    assert_eq!((t.resolved.as_str(), t.schema_exists, t.table_exists), ("main.people", Some(true), Some(true)), "{t:?}");
    assert!(t.respelled && t.problems.is_empty() && t.notes.iter().any(|n| n.contains("Table People is people")), "{t:?}");
    let t = api::check_load_target(&st, &lite, "ACCOUNT_NEW").await.unwrap();
    assert_eq!((t.resolved.as_str(), t.table_exists), ("main.ACCOUNT_NEW", Some(false)), "a new table keeps the typed name");
    let t = api::check_load_target(&st, &lite, "adults").await.unwrap();
    assert!(t.problems[0].contains("is a view"), "{t:?}");
    let t = api::check_load_target(&st, &lite, "a.b.c").await.unwrap();
    assert!(t.problems[0].contains("too many parts"), "{t:?}");
    let t = api::check_load_target(&st, &lite, r#""main"."people""#).await.unwrap();
    assert_eq!(t.table_exists, Some(true), "typed quotes");

    // Three levels (DuckDB: database.schema.table).
    let t = api::check_load_target(&st, &duck, "nodb.main.t").await.unwrap();
    assert_eq!(t.catalog_exists, Some(false));
    assert!(t.problems[0].starts_with("Database nodb doesn't exist"), "{t:?}");
    let t = api::check_load_target(&st, &duck, "noschema.t").await.unwrap();
    assert_eq!((t.catalog_exists, t.schema_exists), (Some(true), Some(false)), "{t:?}");
    assert!(t.create_sql.as_deref().unwrap_or("").starts_with("CREATE SCHEMA "), "{t:?}");
    // A DuckDB file: its database and main schema are the defaults.
    let mut c = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
    c.file_path = Some(dir.path().join("dst.duckdb").to_string_lossy().into_owned());
    let profile = ConnectionProfile {
        id: String::new(),
        name: "dk".into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    let dk = api::save_connection(&st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;
    let t = api::check_load_target(&st, &dk, "items").await.unwrap();
    assert_eq!((t.resolved.as_str(), t.table_exists), ("dst.main.items", Some(false)), "{t:?}");

    // Loading into a missing schema stops before CREATE TABLE, and says why.
    let info = run(&st, &duck, "select 1 as id").await.result.unwrap();
    let args = LoadResultArgs { target_connection_id: lite.clone(), table: "nosuch.t".into(), mode: LoadMode::Append, key_columns: vec![], create_table: true };
    let e = api::load_result(&st, &info.id, args.clone(), "x1").await.unwrap_err();
    assert!(e.message.contains("nosuch doesn't exist") && e.message.contains("ATTACH DATABASE"), "{}", e.message);
    let dry = api::dry_run_result_load(&st, &info.id, args).await.unwrap();
    assert!(!dry.ok && dry.checks[0].message.as_deref().unwrap_or("").contains("doesn't exist"), "{dry:?}");
}

/// Live: rows (with NULLs, a timestamp with an offset, text with quotes and
/// backslashes) load into ClickHouse: a new MergeTree table, Delete rows
/// then insert, Replace; Update/Merge by key are refused before anything
/// runs. Set DATABRAIN_CLICKHOUSE_HOST (+ _PORT, _USER, _PASSWORD).
#[ignore = "live: needs DATABRAIN_CLICKHOUSE_HOST (cargo test -- --ignored)"]
#[tokio::test(flavor = "multi_thread")]
async fn loads_into_clickhouse_live() {
    let Ok(host) = std::env::var("DATABRAIN_CLICKHOUSE_HOST") else {
        panic!("live test: DATABRAIN_CLICKHOUSE_HOST not set (run with --ignored)");
    };
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let duck = st.engine.results_connection().unwrap().id;
    let user = std::env::var("DATABRAIN_CLICKHOUSE_USER").unwrap_or_else(|_| "default".into());
    let mut c = ConnectionConfig::new(ConnectorKind::Clickhouse, AuthMethod::Password { user });
    c.host = Some(host);
    c.port = std::env::var("DATABRAIN_CLICKHOUSE_PORT").ok().and_then(|p| p.parse().ok());
    let profile = ConnectionProfile {
        id: String::new(),
        name: "ch".into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    let secret = std::env::var("DATABRAIN_CLICKHOUSE_PASSWORD").ok();
    let target = api::save_connection(&st, api::SaveConnectionArgs { profile, secret, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;
    run(&st, &target, "CREATE DATABASE IF NOT EXISTS databrain_load").await;
    run(&st, &target, "DROP TABLE IF EXISTS databrain_load.people").await;

    let r = run(
        &st,
        &duck,
        "select i as id, case when i % 3 = 0 then null else 'it''s \\ ' || i end as name, \
         timestamptz '2026-01-02 03:04:05+02' + i * interval 1 hour as at, i % 2 = 0 as even from range(10) t(i)",
    )
    .await;
    let info = r.result.unwrap();
    let args = |mode: LoadMode| LoadResultArgs {
        target_connection_id: target.clone(),
        table: "databrain_load.people".into(),
        mode,
        key_columns: vec!["id".into()],
        create_table: true,
    };
    async fn count(st: &AppState, target: &str) -> (String, String) {
        let r = run(st, target, "select count() as n, countIf(name is null) as nulls from databrain_load.people").await.result.unwrap();
        let p = api::fetch_page(st, r.id, Default::default(), 0, 1).await.unwrap();
        (p.rows[0][0].clone().unwrap(), p.rows[0][1].clone().unwrap())
    }

    let rep = api::load_result(&st, &info.id, args(LoadMode::Append), "c1").await.unwrap();
    assert_eq!(rep.rows, 10);
    assert!(rep.created_table);
    assert_eq!(count(&st, &target).await, ("10".to_string(), "4".to_string()), "NULLs stay NULL, not ''");
    let ddl = run(&st, &target, "show create table databrain_load.people").await.result.unwrap();
    let p = api::fetch_page(&st, ddl.id, Default::default(), 0, 1).await.unwrap();
    assert!(p.rows[0][0].as_deref().unwrap().contains("MergeTree"), "{:?}", p.rows);

    api::load_result(&st, &info.id, args(LoadMode::Truncate), "c2").await.unwrap();
    assert_eq!(count(&st, &target).await.0, "10", "emptied first");
    api::load_result(&st, &info.id, args(LoadMode::Replace), "c3").await.unwrap();
    assert_eq!(count(&st, &target).await.0, "10");

    for mode in [LoadMode::Update, LoadMode::Merge] {
        let e = api::load_result(&st, &info.id, args(mode), "c4").await.unwrap_err();
        assert!(e.message.contains("can't update rows by key"), "{}", e.message);
    }
    run(&st, &target, "DROP TABLE IF EXISTS databrain_load.people").await;
}

/// ClickHouse has no row-level UPDATE/MERGE: Update and Merge by key are
/// refused before connecting, naming the modes that work.
#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_refuses_keyed_loads_up_front() {
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let duck = st.engine.results_connection().unwrap().id;
    let mut c = ConnectionConfig::new(ConnectorKind::Clickhouse, AuthMethod::Password { user: "default".into() });
    c.host = Some("127.0.0.1".into());
    c.port = Some(1);
    let profile = ConnectionProfile {
        id: String::new(),
        name: "ch".into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    let target =
        api::save_connection(&st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;
    let info = run(&st, &duck, "select 1 as id").await.result.unwrap();
    for mode in [LoadMode::Update, LoadMode::Merge] {
        let args = LoadResultArgs { target_connection_id: target.clone(), table: "db.t".into(), mode, key_columns: vec!["id".into()], create_table: true };
        let e = api::load_result(&st, &info.id, args, "k").await.unwrap_err();
        assert!(e.message.contains("ClickHouse can't update rows by key") && e.message.contains("Replace table"), "{}", e.message);
    }
    assert!(st.engine.connected_ids().iter().all(|c| c != &target), "refused before connecting");
}

/// Sync by key: the table ends up holding exactly the rows (changed ones
/// updated, new ones inserted, the others deleted) and the report counts
/// the deletions; a step without rows is refused rather than emptying it.
#[tokio::test(flavor = "multi_thread")]
async fn sync_mirrors_the_rows_by_key() {
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let duck = st.engine.results_connection().unwrap().id;
    let dir = tempfile::tempdir().unwrap();
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(dir.path().join("sync.db").to_string_lossy().into_owned());
    let profile = ConnectionProfile {
        id: String::new(),
        name: "copy".into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    let target =
        api::save_connection(&st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;
    let args = |mode: LoadMode| LoadResultArgs {
        target_connection_id: target.clone(),
        table: "people".into(),
        mode,
        key_columns: vec!["id".into()],
        create_table: true,
    };

    let first = run(&st, &duck, "select i as id, 'name ' || i as name from range(25) t(i)").await.result.unwrap();
    api::load_result(&st, &first.id, args(LoadMode::Append), "s1").await.unwrap();
    // Source now: ids 0..19 (id 3 renamed) and 30..32; ids 20..24 are gone.
    let second = run(
        &st,
        &duck,
        "select i as id, case when i = 3 then 'renamed' else 'name ' || i end as name from range(20) t(i) \
         union all select i, 'new ' || i from range(30, 33) t(i)",
    )
    .await
    .result
    .unwrap();
    let rep = api::load_result(&st, &second.id, args(LoadMode::Sync), "s2").await.unwrap();
    assert_eq!(rep.deleted, 5, "{rep:?}");
    assert!(rep.notices.iter().any(|n| n.contains("5 deleted")), "{:?}", rep.notices);
    let check = run(&st, &target, "select count(*) as n, sum(id) as ids, max(case when id = 3 then name end) as three from people").await.result.unwrap();
    let page = api::fetch_page(&st, check.id, Default::default(), 0, 1).await.unwrap();
    let ids: i64 = (0..20).sum::<i64>() + 30 + 31 + 32;
    assert_eq!(page.rows[0], vec![Some("23".to_string()), Some(ids.to_string()), Some("renamed".to_string())]);

    // No rows: refused, nothing deleted.
    let empty = run(&st, &duck, "select 1 as id, 'x' as name where false").await.result.unwrap();
    let e = api::load_result(&st, &empty.id, args(LoadMode::Sync), "s3").await.unwrap_err();
    assert!(e.message.contains("Sync would delete every row"), "{}", e.message);
    let n = run(&st, &target, "select count(*) from people").await.result.unwrap();
    assert_eq!(api::fetch_page(&st, n.id, Default::default(), 0, 1).await.unwrap().rows[0][0].as_deref(), Some("23"));
}
