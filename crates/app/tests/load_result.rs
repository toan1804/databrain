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
                output_masks: None,
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
        confirmed: false,
        every_row: None,
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
    let args = LoadResultArgs {
        target_connection_id: lite.clone(),
        table: "nosuch.t".into(),
        mode: LoadMode::Append,
        key_columns: vec![],
        create_table: true,
        confirmed: false,
        every_row: None,
    };
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
         (timestamp '2026-01-02 01:04:05' + i * interval 1 hour)::timestamptz as at, i % 2 = 0 as even from range(10) t(i)",
    )
    .await;
    let info = r.result.unwrap_or_else(|| panic!("{:?}", r.error));
    let args = |mode: LoadMode| LoadResultArgs {
        target_connection_id: target.clone(),
        table: "databrain_load.people".into(),
        mode,
        key_columns: vec!["id".into()],
        create_table: true,
        confirmed: false,
        every_row: None,
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
        let args = LoadResultArgs {
            target_connection_id: target.clone(),
            table: "db.t".into(),
            mode,
            key_columns: vec!["id".into()],
            create_table: true,
            confirmed: false,
            every_row: None,
        };
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
        confirmed: false,
        every_row: None,
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

async fn run_as(
    st: &AppState,
    conn: &str,
    sql: &str,
    row_limit: Option<usize>,
    origin: databrain_workspace::Origin,
) -> databrain_query_engine::StatementOutcomeView {
    let out = st
        .engine
        .run_and_wait(
            &st.hub,
            RunRequest {
                connection_id: conn.into(),
                tab_id: "t".into(),
                sql: sql.into(),
                base_offset: 0,
                row_limit,
                confirmed: true,
                origin,
                session_key: None,
                output_name: None,
                params: Default::default(),
                output_masks: None,
            },
            None,
        )
        .await
        .unwrap();
    out.into_iter().next_back().unwrap()
}

/// A result cut short by the row limit: "every row" runs its query again and
/// writes all its rows as they arrive; without it only the rows held load.
/// Only the user's own reads run again.
#[tokio::test(flavor = "multi_thread")]
async fn every_row_runs_a_cut_result_again() {
    use databrain_workspace::Origin;
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
    let sql = "select i as id, 'name ' || i as name from range(30000) t(i)";
    let info = run_as(&st, &duck, sql, Some(100), Origin::User).await.result.unwrap();
    assert!(info.truncated && info.total_rows == 100);
    let args = |every: Option<bool>| LoadResultArgs {
        target_connection_id: target.clone(),
        table: "people".into(),
        mode: LoadMode::Truncate,
        key_columns: vec![],
        create_table: true,
        confirmed: false,
        every_row: every,
    };

    let rep = api::load_result(&st, &info.id, args(None), "held").await.unwrap();
    assert_eq!(rep.rows, 100, "the rows the result holds");
    let rep = api::load_result(&st, &info.id, args(Some(true)), "every").await.unwrap();
    assert_eq!(rep.rows, 30000, "every row of the query");
    let counted = run(&st, &target, "select count(*) as n from people").await.result.unwrap();
    let page = api::fetch_page(&st, counted.id, Default::default(), 0, 1).await.unwrap();
    assert_eq!(page.rows[0][0].as_deref(), Some("30000"));

    // A result the AI ran isn't run again as the user.
    let ai = run_as(&st, &duck, sql, Some(100), Origin::Ai).await.result.unwrap();
    let e = api::load_result(&st, &ai.id, args(Some(true)), "ai").await.unwrap_err();
    assert!(e.message.contains("Only your own queries"), "{}", e.message);
    // Only a read runs again.
    let spec = databrain_query_engine::LoadSpec { table: "people".into(), create_table: true, ..Default::default() };
    let cancel = databrain_connector_core::CancellationToken::new();
    let e = st
        .engine
        .load_query(&duck, "t", "create table x as select 1 as a", &target, "w", &spec, &cancel, &databrain_query_engine::NoLoadObserver)
        .await
        .unwrap_err();
    assert!(e.message.contains("Only a query that reads"), "{}", e.message);
    assert!(run(&st, &duck, "select * from x").await.error.is_some(), "nothing ran");
}

/// A result larger than the memory it may keep continues on disk instead of
/// being cut: every row stays, pages read from the files, results.<name>
/// sees them all (exact decimals as DECIMAL), export and "Load into…" stream
/// them, and freeing the result deletes its files.
#[tokio::test(flavor = "multi_thread")]
async fn results_larger_than_memory_continue_on_disk() {
    use databrain_workspace::Origin;
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let app_dir = tempfile::tempdir().unwrap();
    api::set_results_spill(&st.engine, app_dir.path());
    st.engine.outputs().set_budget(0); // the 16 MB floor: 8 MB per result
    let duck = st.engine.results_connection().unwrap().id;
    let n = 400_000;
    let sql = format!("select i as id, md5(i::varchar) as s, (i / 100)::decimal(12,2) as amount from range({n}) t(i)");
    let out = run_as(&st, &duck, &sql, None, Origin::User).await;
    let info = out.result.clone().unwrap_or_else(|| panic!("{:?}", out.error));
    assert!(!info.truncated && info.total_rows == n, "{} rows, truncated {}", info.total_rows, info.truncated);
    assert!(info.on_disk && info.disk_bytes > 0 && info.bytes < 8 << 20, "{info:?}");
    assert!(out.notices.iter().any(|m| m.contains("on disk")), "{:?}", out.notices);

    // Any page, read from the files.
    let page = api::fetch_page(&st, info.id.clone(), Default::default(), 350_000, 3).await.unwrap();
    assert_eq!(page.rows.iter().map(|r| r[0].clone().unwrap()).collect::<Vec<_>>(), vec!["350000", "350001", "350002"]);
    assert_eq!(page.row_ids, vec![350_000, 350_001, 350_002]);
    // Sort and filter: of the rows the grid has loaded (the first 1,000),
    // in memory; none given, of every row that fits in the window.
    use databrain_result_store::{ColumnFilter, FilterOp, SortKey, ViewSpec};
    let keys = vec![SortKey { column: 2, descending: true }, SortKey { column: 0, descending: false }];
    let sorted = ViewSpec { sort: keys.clone(), first_rows: Some(1000), ..Default::default() };
    let page = api::fetch_page(&st, info.id.clone(), sorted.clone(), 0, 3).await.unwrap();
    assert_eq!((page.view_rows, page.total_rows, page.window_rows), (1000, n, Some(1000)));
    // amount = id / 100 (2 decimals): the largest of the first 1,000 are 9.99 (999), 9.98 (998)...
    assert_eq!(page.rows.iter().map(|r| r[0].clone().unwrap()).collect::<Vec<_>>(), vec!["999", "998", "997"]);
    assert_eq!(page.row_ids, vec![999, 998, 997]);
    let every = ViewSpec { sort: keys.clone(), ..Default::default() };
    let page = api::fetch_page(&st, info.id.clone(), every, 0, 1).await.unwrap();
    assert_eq!((page.view_rows, page.window_rows, page.row_ids.clone()), (n, Some(n), vec![399_999]));
    let filter = vec![ColumnFilter { column: 0, op: FilterOp::Gte, value: "399990".into() }];
    let loaded = ViewSpec { filters: filter.clone(), first_rows: Some(1000), ..Default::default() };
    assert_eq!(api::fetch_page(&st, info.id.clone(), loaded, 0, 5).await.unwrap().view_rows, 0, "rows past the first 1,000 aren't filtered");
    let filtered = ViewSpec { filters: filter, quick_filter: Some("9".into()), ..Default::default() };
    let page = api::fetch_page(&st, info.id.clone(), filtered.clone(), 0, 20).await.unwrap();
    assert_eq!(page.view_rows, 10, "ids 399990..=399999 all contain a 9");
    assert_eq!(page.row_ids.first(), Some(&399_990));
    let found = api::find_in_result(&st, info.id.clone(), filtered, "39999".into(), Some(vec![0]), 100, false).await.unwrap();
    assert_eq!(found.matches.len(), 10);
    // Find in the plain result: every row, from the files.
    let found = api::find_in_result(&st, info.id.clone(), ViewSpec::default(), "399999".into(), Some(vec![0]), 100, false).await.unwrap();
    assert_eq!(found.matches.len(), 1);
    // Column statistics: of the rows loaded in the grid (in memory), not the files.
    let loaded = ViewSpec { first_rows: Some(1000), ..Default::default() };
    let stats = api::column_stats(&st, info.id.clone(), loaded, 0).await.unwrap();
    assert_eq!((stats.count, stats.nulls, stats.distinct), (1000, 0, 1000));
    assert_eq!((stats.min.as_deref(), stats.max.as_deref()), (Some("0"), Some("999")));
    // Export of a sorted view: its rows, in that order.
    let top = app_dir.path().join("top.csv");
    api::export_result(
        &st,
        info.id.clone(),
        sorted,
        databrain_export::ExportOptions::new(databrain_export::ExportFormat::Csv),
        top.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    let csv = std::fs::read_to_string(&top).unwrap();
    assert!(csv.lines().nth(1).unwrap().starts_with("999,"), "{}", csv.lines().nth(1).unwrap());
    assert_eq!(csv.lines().count(), 1001, "a header and the 1,000 rows");

    // results.<handle>: every row, the decimal as a number.
    let handle = st.engine.outputs().by_result(&info.id).unwrap().handle;

    // A chart: of the rows loaded in the grid (in memory), saying how many.
    let count = databrain_result_store::ChartSpec { x: 0, y: vec![], agg: databrain_result_store::Agg::Count, series: None, limit: 5 };
    let chart = api::chart_data(&st, info.id.clone(), ViewSpec { first_rows: Some(1000), ..Default::default() }, count).await.unwrap();
    assert_eq!((chart.rows, chart.window_rows), (1000, Some(1000)));
    // Pinning (a saved copy of every row): refused, saying why.
    assert!(st.engine.outputs().get(&handle).unwrap().spilled);
    let e = api::pin_output(&st, handle.clone(), true).await.unwrap_err();
    assert!(e.message.starts_with("Too large to pin") && e.message.contains("Export it"), "{}", e.message);
    assert!(!st.engine.outputs().get(&handle).unwrap().pinned);
    let agg = run_as(&st, &duck, &format!("select count(*), sum(amount), typeof(amount) from results.{handle} group by 3"), None, Origin::User).await;
    let agg = agg.result.unwrap_or_else(|| panic!("{:?}", agg.error));
    let row = api::fetch_page(&st, agg.id, Default::default(), 0, 1).await.unwrap().rows.remove(0);
    // `i / 100` is exact division in DuckDB: Σi / 100, a whole number here.
    let expected_sum: i64 = (0..n as i64).sum::<i64>() / 100;
    assert_eq!(row[0].as_deref(), Some(n.to_string().as_str()));
    assert_eq!(row[1].as_deref().map(|s| s.trim_end_matches(".00")), Some(expected_sum.to_string().as_str()));
    assert!(row[2].as_deref().unwrap().starts_with("DECIMAL"), "{row:?}");

    // With masks (the assistant's statements): streamed in, the column hidden.
    let masks = databrain_connector_core::external::OutputMasks(std::collections::HashMap::from([(info.id.clone(), vec!["s".to_string()])]));
    let masked = st
        .engine
        .run_and_wait(
            &st.hub,
            RunRequest {
                connection_id: duck.clone(),
                tab_id: "masked".into(),
                sql: format!("select count(*), count(distinct s), min(s) from results.{handle}"),
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin: Default::default(),
                session_key: None,
                output_name: None,
                params: Default::default(),
                output_masks: Some(Arc::new(masks)),
            },
            None,
        )
        .await
        .unwrap()
        .remove(0);
    let masked = masked.result.unwrap_or_else(|| panic!("{:?}", masked.error));
    let row = api::fetch_page(&st, masked.id, Default::default(), 0, 1).await.unwrap().rows.remove(0);
    assert_eq!(row, vec![Some(n.to_string()), Some("1".into()), Some("***".into())]);

    // Export: every row, streamed.
    let csv = app_dir.path().join("all.csv");
    let wrote = api::export_result(
        &st,
        info.id.clone(),
        Default::default(),
        databrain_export::ExportOptions::new(databrain_export::ExportFormat::Csv),
        csv.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    assert_eq!(wrote, n as u64);
    assert_eq!(std::fs::read_to_string(&csv).unwrap().lines().count(), n + 1);

    // Load into another connection: every row, streamed.
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
    let args = LoadResultArgs {
        target_connection_id: target,
        table: "big".into(),
        mode: LoadMode::Append,
        key_columns: vec![],
        create_table: true,
        confirmed: false,
        every_row: None,
    };
    assert_eq!(api::load_result(&st, &info.id, args, "disk").await.unwrap().rows, n as u64);

    // Freed: the files go too.
    let spill_dir = st.engine.results().spill_dir(&info.id).unwrap();
    assert!(spill_dir.is_dir());
    assert!(api::release_result(&st, &info.id));
    assert!(!spill_dir.exists(), "{}", spill_dir.display());
}

/// Folders left by a process that quit are deleted at start; a live
/// process's folder (its lock held) is kept.
#[test]
fn stale_spill_folders_are_deleted_live_ones_kept() {
    let base = tempfile::tempdir().unwrap();
    let stale = base.path().join("results_spill").join("123-1");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("000000.parquet"), b"x").unwrap();
    // Left by a process that quit a while ago (a folder created this minute may be starting up).
    // (Unix only: Windows can't open a folder to set its time.)
    #[cfg(unix)]
    std::fs::File::open(&stale).unwrap().set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600)).unwrap();
    let fresh = base.path().join("results_spill").join("456-1");
    std::fs::create_dir_all(&fresh).unwrap();
    let live = databrain_result_store::ResultStore::new();
    live.set_spill_root(base.path().join("results_spill"));
    let live_dir = live.spill_root().unwrap();
    #[cfg(unix)]
    assert!(!stale.exists(), "nobody held it");
    assert!(fresh.exists(), "just created: its process may be about to lock it");
    let other = databrain_result_store::ResultStore::new();
    other.set_spill_root(base.path().join("results_spill"));
    assert!(live_dir.exists(), "held by the first store");
    assert_ne!(other.spill_root().unwrap(), live_dir);
}
