//! Jobs end to end: a SQLite query step, a DuckDB step reading its output,
//! and a load step writing into another SQLite database.

use std::sync::Arc;
use std::time::Duration;

use databrain_app::api::{self, AppState, SaveConnectionArgs};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::{EventSink, JobEvent, RunRequest};
use databrain_workspace::{FileFormat, ConnectionProfile, EnvTag, Job, JobEdge, JobNode, JobNodeKind, JobRun, JobSchedule, LoadMode, Workspace};

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

fn state() -> AppState {
    AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi))
}

fn sqlite(st: &AppState, name: &str, path: &std::path::Path, read_only: bool) -> String {
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(path.to_string_lossy().into_owned());
    c.read_only = read_only;
    api::save_connection(
        st,
        SaveConnectionArgs {
            profile: ConnectionProfile { id: String::new(), name: name.into(), config: c, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 },
            secret: None,
            clear_secret: false,
            extra_secrets: Default::default(),
        },
    )
    .unwrap()
    .id
}

fn node(id: &str, name: &str, kind: JobNodeKind, conn: Option<&str>, sql: &str) -> JobNode {
    JobNode {
        id: id.into(),
        name: name.into(),
        kind,
        connection_id: conn.map(String::from),
        sql: sql.into(),
        x: 0.0,
        y: 0.0,
        target_connection_id: None,
        target_table: None,
        load_mode: LoadMode::Append,
        load_before_sql: None,
        load_after_sql: None,
        key_columns: vec![],
        create_table: true,
        batch_rows: None,
        batch_kb: None,
        export_folder: None,
        export_file: None,
        export_format: FileFormat::Csv,
        last_run: None,
    }
}

async fn finished(st: &AppState, job: &str) -> JobRun {
    for _ in 0..400 {
        let run = st.workspace.job_runs(job, 1).unwrap().remove(0);
        if run.status != "running" {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job did not finish");
}

/// Rows of `sql` on a connection, as text.
async fn rows(st: &AppState, conn: &str, sql: &str) -> Vec<Vec<String>> {
    let out = st
        .engine
        .run_and_wait(&st.hub, RunRequest { connection_id: conn.into(), tab_id: format!("t-{}", uuid::Uuid::new_v4()), sql: sql.into(), base_offset: 0, row_limit: None, confirmed: true, origin: Default::default(), session_key: None, output_name: None, params: Default::default() }, None)
        .await
        .unwrap();
    if let Some(e) = out.iter().find_map(|o| o.error.as_ref()) {
        panic!("{}: {sql}", e.message);
    }
    let id = out.iter().rev().find_map(|o| o.result.clone()).expect("no result").id;
    let page = api::fetch_page(st, id, Default::default(), 0, 1000).await.unwrap();
    page.rows.into_iter().map(|r| r.into_iter().map(|c| c.unwrap_or_default()).collect()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn query_duckdb_and_load_steps() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
    let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
    rows(&st, &src, "create table orders (id integer, region text, amount real); insert into orders values (1, 'eu', 10.5), (2, 'eu', 4.5), (3, 'us', 7); select 1").await;

    let mut load = node("c", "totals", JobNodeKind::Load, None, "");
    load.target_connection_id = Some(dst.clone());
    load.target_table = Some("region_totals".into());
    load.load_mode = LoadMode::Replace;
    let job = st
        .workspace
        .save_job(Job {
            id: String::new(),
            name: "Nightly".into(),
            nodes: vec![
                node("a", "orders", JobNodeKind::Query, Some(&src), "select region, amount from orders"),
                node("b", "by_region", JobNodeKind::Query, None, "select region, sum(amount) as total, count(*) as n from results.orders group by region order by region"),
                load,
            ],
            edges: vec![JobEdge { from: "a".into(), to: "b".into() }, JobEdge { from: "b".into(), to: "c".into() }],
            schedule: JobSchedule { enabled: true, minutes: 60, ..Default::default() },
            last_scheduled_at: None,
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();

    api::run_job(&st, &job.id, None).unwrap();
    assert!(api::run_job(&st, &job.id, None).is_err(), "one run at a time");
    let run = finished(&st, &job.id).await;
    assert_eq!(run.status, "success", "{run:?}");
    let by = |n: &str| run.nodes.iter().find(|r| r.name == n).unwrap().summary.clone();
    assert_eq!((by("orders").rows, by("by_region").rows, by("totals").rows), (Some(3), Some(2), Some(2)));
    assert_eq!(rows(&st, &dst, "select region, total, n from region_totals order by region").await, vec![vec!["eu", "15.0", "2"], vec!["us", "7.0", "1"]]);
    assert_eq!(st.workspace.get_job(&job.id).unwrap().nodes[2].last_run.as_ref().unwrap().rows, Some(2));

    // Append adds the rows again; Replace (above) recreated the table.
    let mut j = st.workspace.get_job(&job.id).unwrap();
    j.nodes[2].load_mode = LoadMode::Append;
    st.workspace.save_job(j).unwrap();
    api::run_job(&st, &job.id, Some(vec!["c".into()])).unwrap();
    let run = finished(&st, &job.id).await;
    assert_eq!(run.nodes.len(), 1, "only the chosen step ran");
    assert_eq!(rows(&st, &dst, "select count(*) from region_totals").await, vec![vec!["4"]]);

    // Enabling a schedule doesn't run the job at once.
    assert!(databrain_app::jobs::tick(&st.job_ctx()).is_empty());
    assert!(api::list_jobs(&st).unwrap().jobs[0].next_run_at.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_skip_downstream_and_read_only_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
    let ro = sqlite(&st, "ro", &dir.path().join("ro.db"), true);
    let mut load = node("d", "out", JobNodeKind::Load, None, "select 1 as x");
    load.target_connection_id = Some(ro);
    load.target_table = Some("t".into());
    let job = st
        .workspace
        .save_job(Job {
            id: String::new(),
            name: "Broken".into(),
            nodes: vec![
                node("a", "bad", JobNodeKind::Query, Some(&src), "select * from missing_table"),
                node("b", "after_bad", JobNodeKind::Query, None, "select * from results.bad"),
                node("c", "fine", JobNodeKind::Query, Some(&src), "select 1 as one"),
                load,
            ],
            edges: vec![JobEdge { from: "a".into(), to: "b".into() }],
            schedule: Default::default(),
            last_scheduled_at: None,
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    api::run_job(&st, &job.id, None).unwrap();
    let run = finished(&st, &job.id).await;
    let status = |n: &str| run.nodes.iter().find(|r| r.name == n).unwrap().summary.status.clone();
    assert_eq!(run.status, "error");
    assert_eq!((status("bad"), status("after_bad"), status("fine"), status("out")), ("error".into(), "skipped".into(), "success".into(), "error".into()));
    let err = run.nodes.iter().find(|r| r.name == "out").unwrap().summary.error.clone().unwrap();
    assert!(err.contains("read-only"), "{err}");
    assert!(run.error.unwrap().starts_with("bad:"));
}

/// Load step into Postgres / MySQL (skipped unless `DATABRAIN_PG_HOST` /
/// `DATABRAIN_MYSQL_HOST` are set): types, quoting, NULLs, Replace + Truncate.
#[tokio::test(flavor = "multi_thread")]
async fn load_into_server_databases() {
    let targets = [("DATABRAIN_PG", ConnectorKind::Postgres, "postgres", "5432"), ("DATABRAIN_MYSQL", ConnectorKind::Mysql, "root", "3306")];
    for (env, kind, user, port) in targets {
        let Ok(host) = std::env::var(format!("{env}_HOST")) else { continue };
        let st = state();
        let mut c = ConnectionConfig::new(kind, AuthMethod::Password { user: std::env::var(format!("{env}_USER")).unwrap_or(user.into()) });
        c.host = Some(host);
        c.port = std::env::var(format!("{env}_PORT")).unwrap_or(port.into()).parse().ok();
        c.database = Some(if kind == ConnectorKind::Postgres { "postgres".into() } else { "mysql".into() });
        let id = api::save_connection(
            &st,
            SaveConnectionArgs {
                profile: ConnectionProfile { id: String::new(), name: "db".into(), config: c, color: None, env: EnvTag::None, folder_id: None, has_secret: true, ai_policy: Default::default(), created_at: 0, updated_at: 0 },
                secret: std::env::var(format!("{env}_PASSWORD")).ok(),
                clear_secret: false,
                extra_secrets: Default::default(),
            },
        )
        .unwrap()
        .id;
        let mut load = node("b", "loaded", JobNodeKind::Load, None, "");
        load.target_connection_id = Some(id.clone());
        load.target_table = Some("databrain_job_load".into());
        load.load_mode = LoadMode::Replace;
        let src = "select 1 as id, 'it''s \"x\"' as name, true as ok, 2.5 as amount, timestamp '2026-01-02 03:04:05' as at, null::varchar as missing \
                   union all select 2, 'b', false, null, null, 'm'";
        let job = st
            .workspace
            .save_job(Job {
                id: String::new(),
                name: "load".into(),
                nodes: vec![node("a", "src_rows", JobNodeKind::Query, None, src), load],
                edges: vec![JobEdge { from: "a".into(), to: "b".into() }],
                schedule: Default::default(),
                last_scheduled_at: None,
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        for mode in [LoadMode::Replace, LoadMode::Truncate, LoadMode::Merge, LoadMode::Update] {
            let mut j = st.workspace.get_job(&job.id).unwrap();
            j.nodes[1].load_mode = mode;
            j.nodes[1].key_columns = vec!["id".into()];
            st.workspace.save_job(j).unwrap();
            api::run_job(&st, &job.id, None).unwrap();
            let run = finished(&st, &job.id).await;
            assert_eq!(run.status, "success", "{kind:?} {mode:?}: {:?}", run.error);
        }
        let got = rows(&st, &id, "select id, name, amount, missing from databrain_job_load order by id").await;
        assert_eq!(got.len(), 2, "{kind:?}: truncate kept 2 rows");
        assert_eq!(got[0][1], "it's \"x\"", "{kind:?}");
        assert_eq!(got[1][3], "m");
        rows(&st, &id, "drop table databrain_job_load; select 1").await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn export_steps_write_files_with_duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    let mut nodes = vec![node("a", "src_rows", JobNodeKind::Query, None, "select 1 as id, 'it''s, quoted' as name union all select 2, 'b'")];
    let mut edges = vec![];
    for (i, (fmt, file)) in [(FileFormat::Csv, Some("report {step}")), (FileFormat::Parquet, None), (FileFormat::Json, Some("rows.json"))].into_iter().enumerate() {
        let id = format!("e{i}");
        let mut e = node(&id, &format!("out_{i}"), JobNodeKind::Export, None, "");
        e.export_folder = Some(dir.path().to_string_lossy().into_owned());
        e.export_file = file.map(String::from);
        e.export_format = fmt;
        nodes.push(e);
        edges.push(JobEdge { from: "a".into(), to: id });
    }
    let mut missing = node("m", "nowhere", JobNodeKind::Export, None, "select 1");
    missing.export_folder = Some(dir.path().join("nope").to_string_lossy().into_owned());
    nodes.push(missing);
    let job = st
        .workspace
        .save_job(Job { id: String::new(), name: "Export".into(), nodes, edges, schedule: Default::default(), last_scheduled_at: None, created_at: 0, updated_at: 0 })
        .unwrap();
    api::run_job(&st, &job.id, None).unwrap();
    let run = finished(&st, &job.id).await;
    let rec = |n: &str| run.nodes.iter().find(|r| r.name == n).unwrap().summary.clone();
    assert!(rec("nowhere").error.unwrap().contains("Folder not found"));
    let csv = rec("out_0").file.unwrap();
    assert!(csv.ends_with("report out_0.csv"), "{csv}");
    assert_eq!(std::fs::read_to_string(&csv).unwrap(), "id,name\n1,\"it's, quoted\"\n2,b\n");
    let pq = rec("out_1").file.unwrap();
    assert!(pq.ends_with(".parquet") && pq.contains("out_1_"), "{pq}");
    assert_eq!(rec("out_1").rows, Some(2));
    let duck = st.engine.results_connection().unwrap().id;
    assert_eq!(rows(&st, &duck, &format!("select count(*) from read_parquet('{pq}')")).await, vec![vec!["2"]]);
    let json = std::fs::read_to_string(rec("out_2").file.unwrap()).unwrap();
    assert_eq!(json.lines().count(), 2);
    assert!(json.contains("\"name\":\"b\""), "{json}");
}

fn plain_job(name: &str, nodes: Vec<JobNode>) -> Job {
    Job { id: String::new(), name: name.into(), nodes, edges: vec![], schedule: Default::default(), last_scheduled_at: None, created_at: 0, updated_at: 0 }
}

#[tokio::test(flavor = "multi_thread")]
async fn step_names_own_their_output_names() {
    let st = state();
    let duck = st.engine.results_connection().unwrap().id;
    // A query tab already produced `revenue`: a step can't take that name.
    let out = st
        .engine
        .run_and_wait(&st.hub, RunRequest { connection_id: duck.clone(), tab_id: "tab1".into(), sql: "select 1 as x".into(), base_offset: 0, row_limit: None, confirmed: true, origin: Default::default(), session_key: None, output_name: Some("revenue".into()), params: Default::default() }, None)
        .await
        .unwrap();
    assert!(out[0].error.is_none());
    let err = api::save_job(&st, plain_job("A", vec![node("a", "Revenue", JobNodeKind::Query, None, "select 1")])).unwrap_err().message;
    assert!(err.contains("query tab"), "{err}");

    // A step owns `sales`: other jobs, tabs and renames can't use it.
    let job = api::save_job(&st, plain_job("Nightly", vec![node("a", "sales", JobNodeKind::Query, None, "select 2 as y")])).unwrap();
    let err = api::save_job(&st, plain_job("B", vec![node("b", "SALES", JobNodeKind::Query, None, "select 1")])).unwrap_err().message;
    assert!(err.contains("job \"Nightly\""), "{err}");
    let run = |tab: &str| RunRequest { connection_id: duck.clone(), tab_id: tab.into(), sql: "select 3".into(), base_offset: 0, row_limit: None, confirmed: true, origin: Default::default(), session_key: None, output_name: Some("sales".into()), params: Default::default() };
    let err = st.engine.run(run("tab2")).unwrap_err().message;
    assert!(err.contains("only that step can produce it"), "{err}");
    let handle = st.engine.outputs().resolve("revenue").unwrap().handle;
    assert!(api::rename_output(&st, &handle, Some("sales".into())).is_err());
    assert_eq!(api::output_name_user(&st, "sales", "").unwrap().unwrap(), "step \"sales\" of job \"Nightly\"");
    assert_eq!(api::output_name_user(&st, "sales", &job.id).unwrap(), None);

    // The step itself still produces it, and the job saves again with its name.
    api::run_job(&st, &job.id, None).unwrap();
    assert_eq!(finished(&st, &job.id).await.status, "success");
    assert_eq!(st.engine.outputs().resolve("sales").unwrap().rows, 1);
    api::save_job(&st, st.workspace.get_job(&job.id).unwrap()).unwrap();
    // Renamed or deleted, the name is free again.
    api::delete_job(&st, &job.id).unwrap();
    assert_eq!(api::output_name_user(&st, "sales", "").unwrap(), None);
    api::save_job(&st, plain_job("C", vec![node("c", "sales", JobNodeKind::Query, None, "select 1")])).unwrap();
}

fn duckdb(st: &AppState, name: &str, path: &std::path::Path) -> String {
    let mut c = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
    c.file_path = Some(path.to_string_lossy().into_owned());
    api::save_connection(
        st,
        SaveConnectionArgs {
            profile: ConnectionProfile { id: String::new(), name: name.into(), config: c, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 },
            secret: None,
            clear_secret: false,
            extra_secrets: Default::default(),
        },
    )
    .unwrap()
    .id
}

/// A job: `src_rows` (DuckDB rows) → `load` into `table` of `target`.
fn load_job(st: &AppState, target: &str, table: &str, src: &str, edit: impl FnOnce(&mut JobNode)) -> Job {
    // Step names are unique in the app: one load job at a time.
    for j in st.workspace.list_jobs().unwrap() {
        st.workspace.delete_job(&j.id).unwrap();
    }
    let mut load = node("b", "loaded", JobNodeKind::Load, None, "");
    load.target_connection_id = Some(target.into());
    load.target_table = Some(table.into());
    edit(&mut load);
    st.workspace
        .save_job(Job {
            id: String::new(),
            name: format!("load {}", uuid::Uuid::new_v4()),
            nodes: vec![node("a", &format!("src_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]), JobNodeKind::Query, None, src), load],
            edges: vec![JobEdge { from: "a".into(), to: "b".into() }],
            schedule: Default::default(),
            last_scheduled_at: None,
            created_at: 0,
            updated_at: 0,
        })
        .unwrap()
}

async fn run_all(st: &AppState, job: &Job) -> JobRun {
    api::run_job(st, &job.id, None).unwrap();
    finished(st, &job.id).await
}

fn step(run: &JobRun, name: &str) -> databrain_workspace::NodeRunSummary {
    run.nodes.iter().find(|r| r.name == name).unwrap().summary.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn load_creates_missing_table_and_runs_before_and_after_sql() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
    let job = load_job(&st, &dst, "sales", "select 1 as id, 'a' as name union all select 2, 'b'", |n| {
        n.load_before_sql = Some("create table if not exists load_log (what text, n integer);\ninsert into load_log values ('before', (select count(*) from sqlite_master where name = 'sales'))".into());
        n.load_after_sql = Some("insert into load_log select 'after', count(*) from sales".into());
    });
    let run = run_all(&st, &job).await;
    assert_eq!(run.status, "success", "{run:?}");
    let s = step(&run, "loaded");
    assert_eq!(s.rows, Some(2));
    assert!(s.notices.iter().any(|n| n.starts_with("Created table sales in dst from the rows' columns: id")), "{:?}", s.notices);
    assert_eq!(rows(&st, &dst, "select what, n from load_log order by rowid").await, vec![vec!["before", "0"], vec!["after", "2"]]);
    // The second run appends into the table that now exists: no notice.
    let run = run_all(&st, &job).await;
    assert!(step(&run, "loaded").notices.is_empty());
    assert_eq!(rows(&st, &dst, "select count(*) from sales").await, vec![vec!["4"]]);

    // Without "create the table", a missing table fails before anything runs.
    let job = load_job(&st, &dst, "missing", "select 1 as id", |n| n.create_table = false);
    let run = run_all(&st, &job).await;
    let err = step(&run, "loaded").error.unwrap();
    assert!(err.contains("doesn't exist") && err.contains("Create the table"), "{err}");
    // Columns the table doesn't have are named.
    let job = load_job(&st, &dst, "sales", "select 3 as id, 'x' as nope", |_| {});
    let err = step(&run_all(&st, &job).await, "loaded").error.unwrap();
    assert!(err.contains("has no column nope") && err.contains("its columns: id, name"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_load_is_rolled_back() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
    rows(&st, &dst, "create table t (id integer, name text); insert into t values (1, 'kept'), (2, 'kept'); select 1").await;
    let job = load_job(&st, &dst, "t", "select 9 as id, 'new' as name", |n| {
        n.load_mode = LoadMode::Truncate;
        n.load_after_sql = Some("insert into no_such_table values (1)".into());
    });
    let run = run_all(&st, &job).await;
    let err = step(&run, "loaded").error.unwrap();
    assert!(err.starts_with("After SQL 1:") && err.contains("rolled back"), "{err}");
    assert_eq!(rows(&st, &dst, "select id, name from t order by id").await, vec![vec!["1", "kept"], vec!["2", "kept"]]);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_and_merge_by_key_columns() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    let targets = [sqlite(&st, "sq", &dir.path().join("dst.db"), false), duckdb(&st, "dk", &dir.path().join("dst.duckdb"))];
    for dst in targets {
        rows(&st, &dst, "create table items (id integer primary key, name text, qty integer, note text); insert into items values (1, 'a', 1, 'n1'), (2, 'b', 2, 'n2'); select 1").await;
        let src = "select 2 as ID, 'B' as name, 20 as qty union all select 3, 'c', 30";
        // Update: only id 2 changes (columns matched ignoring case); id 3 is ignored; `note` is kept.
        let job = load_job(&st, &dst, "items", src, |n| {
            n.load_mode = LoadMode::Update;
            n.key_columns = vec!["id".into()];
        });
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{dst}: {run:?}");
        assert_eq!(step(&run, "loaded").rows, Some(1), "{dst}: rows updated");
        assert_eq!(
            rows(&st, &dst, "select id, name, qty, note from items order by id").await,
            vec![vec!["1", "a", "1", "n1"], vec!["2", "B", "20", "n2"]]
        );
        // Merge: id 2 updated again, id 3 inserted.
        let job = load_job(&st, &dst, "items", "select 2 as id, 'bb' as name, 22 as qty union all select 3, 'c', 30", |n| {
            n.load_mode = LoadMode::Merge;
            n.key_columns = vec!["id".into()];
        });
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{dst}: {run:?}");
        assert_eq!(step(&run, "loaded").rows, Some(2));
        assert_eq!(rows(&st, &dst, "select id, name, qty from items order by id").await, vec![vec!["1", "a", "1"], vec!["2", "bb", "22"], vec!["3", "c", "30"]]);

        // Update/Merge need the table and the keys.
        let job = load_job(&st, &dst, "nope", src, |n| {
            n.load_mode = LoadMode::Merge;
            n.key_columns = vec!["id".into()];
        });
        let err = step(&run_all(&st, &job).await, "loaded").error.unwrap();
        assert!(err.contains("Merge needs an existing table"), "{err}");
        let job = load_job(&st, &dst, "items", src, |n| n.load_mode = LoadMode::Update);
        assert!(step(&run_all(&st, &job).await, "loaded").error.unwrap().contains("key columns"));
        let job = load_job(&st, &dst, "items", src, |n| {
            n.load_mode = LoadMode::Update;
            n.key_columns = vec!["sku".into()];
        });
        assert!(step(&run_all(&st, &job).await, "loaded").error.unwrap().contains("Key column sku"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_checks_without_keeping_changes() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
    rows(&st, &dst, "create table t (id integer, name text); insert into t values (1, 'kept'); select 1").await;

    // Upstream not run yet: the rows can't be selected, the rest is still checked.
    let job = load_job(&st, &dst, "fresh", "select 1 as id, 'a' as name", |n| n.load_before_sql = Some("delete from t".into()));
    let r = api::dry_run_job_step(&st, job.clone(), "b").await.unwrap();
    assert!(!r.ok && r.rows_error.as_deref().unwrap().contains("upstream"), "{r:?}");
    assert_eq!(r.method, "transaction");
    assert_eq!(r.checks[0].status, "ok", "{r:?}");
    assert!(r.checks.iter().any(|c| c.status == "unchecked" && c.label == "Insert rows"), "{r:?}");

    // After the upstream ran: everything runs on the sample rows and is rolled back.
    api::run_job(&st, &job.id, Some(vec!["a".into()])).unwrap();
    finished(&st, &job.id).await;
    let r = api::dry_run_job_step(&st, job.clone(), "b").await.unwrap();
    assert!(r.ok, "{r:?}");
    assert_eq!((r.table_exists, r.creates_table, r.sample_rows), (Some(false), true, Some(1)));
    assert_eq!(r.checks.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), vec!["Before SQL 1", "Create table", "Insert rows"]);
    assert!(r.notices.iter().any(|n| n.contains("fresh doesn't exist in dst")), "{r:?}");
    assert_eq!(rows(&st, &dst, "select count(*) from sqlite_master where name = 'fresh'").await, vec![vec!["0"]], "not created");
    assert_eq!(rows(&st, &dst, "select count(*) from t").await, vec![vec!["1"]], "before SQL rolled back");

    // A failing statement is reported with its message; the later ones are not run.
    let mut bad = job.clone();
    bad.nodes[1].load_after_sql = Some("update missing_table set x = 1".into());
    let r = api::dry_run_job_step(&st, bad, "b").await.unwrap();
    assert!(!r.ok);
    let last = r.checks.last().unwrap();
    assert_eq!((last.label.as_str(), last.status.as_str()), ("After SQL 1", "error"));
    assert!(last.message.as_deref().unwrap().contains("missing_table"), "{last:?}");

    // Plan errors (Update without keys) come back as one check.
    let mut nokeys = job.clone();
    nokeys.nodes[1].load_mode = LoadMode::Update;
    nokeys.nodes[1].target_table = Some("t".into());
    let r = api::dry_run_job_step(&st, nokeys, "b").await.unwrap();
    assert_eq!(r.method, "none");
    assert!(r.checks[0].message.as_deref().unwrap().contains("key columns"));
}

/// UI bridge that keeps every event.
#[derive(Default)]
struct Recorder(parking_lot::Mutex<Vec<(String, serde_json::Value)>>);
impl databrain_app::ai_api::UiBridge for Recorder {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        self.0.lock().push((event.into(), payload));
    }
    fn open_url(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_log_each_statement_and_report_insert_progress() {
    let dir = tempfile::tempdir().unwrap();
    let ui = Arc::new(Recorder::default());
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), ui.clone());
    let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
    // 1,200 rows: three INSERT batches of at most 500.
    let job = load_job(&st, &dst, "big", "select range as id, 'r' || range as name from range(1200)", |n| {
        n.load_before_sql = Some("create table if not exists audit (n integer)".into());
        n.load_after_sql = Some("insert into audit select count(*) from big".into());
    });
    let run = run_all(&st, &job).await;
    assert_eq!(run.status, "success", "{run:?}");
    assert_eq!(step(&run, "loaded").rows, Some(1200));

    let log = api::job_run_log(&st, &job.id, run.id).unwrap();
    let text: Vec<String> = log.iter().map(|e| format!("{} | {} | {}", e.step.clone().unwrap_or_default(), e.level, e.message)).collect();
    let has = |p: &str| text.iter().any(|t| t.contains(p));
    assert!(text[0].contains("Run of") && text[0].contains("2 steps"), "{text:#?}");
    assert!(has("loaded | info | Started (load into a connection)"), "{text:#?}");
    assert!(has("Statement: 1200 rows, 2 columns"), "{text:#?}");
    assert!(has("Before SQL 1: done"), "{text:#?}");
    assert!(has("Insert rows: 1200 rows into big in 3 batches"), "{text:#?}");
    assert!(has("After SQL 1: done, 1 rows affected"), "{text:#?}");
    assert!(has("Commit: done"), "{text:#?}");
    assert!(has("loaded | success | Succeeded: 1200 rows"), "{text:#?}");
    assert!(text.last().unwrap().contains("Run succeeded: 2 succeeded, 0 failed"), "{text:#?}");
    let before = log.iter().find(|e| e.message.starts_with("Before SQL 1")).unwrap();
    assert_eq!(before.sql.as_deref(), Some("create table if not exists audit (n integer)"));
    assert!(log.windows(2).all(|w| w[0].seq < w[1].seq));

    // Live: log lines as events, and the step's progress reaching 1200 / 1200.
    let events = ui.0.lock().clone();
    assert!(events.iter().any(|(e, p)| e == "job-run-log" && p["entries"][0]["message"].as_str().is_some_and(|m| m.starts_with("Insert rows"))));
    let progress: Vec<(u64, u64)> = events
        .iter()
        .filter(|(e, _)| e == "job-run")
        .filter_map(|(_, p)| p["run"]["nodes"].as_array()?.iter().find(|n| n["name"] == "loaded")?.get("progress").map(|g| (g["done"].as_u64().unwrap(), g["total"].as_u64().unwrap())))
        .collect();
    assert_eq!(progress.last(), Some(&(1200, 1200)), "{progress:?}");

    // A failing statement is logged with its SQL and error.
    let mut j = st.workspace.get_job(&job.id).unwrap();
    j.nodes[1].load_after_sql = Some("insert into nowhere values (1)".into());
    st.workspace.save_job(j).unwrap();
    let run = run_all(&st, &job).await;
    let log = api::job_run_log(&st, &job.id, run.id).unwrap();
    let failed = log.iter().find(|e| e.level == "error" && e.message.starts_with("After SQL 1 failed")).expect("after SQL error logged");
    assert_eq!(failed.sql.as_deref(), Some("insert into nowhere values (1)"));
    assert!(log.iter().any(|e| e.message.contains("Rolling back")));
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_per_insert_batch_is_configurable() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
    let job = load_job(&st, &dst, "batched", "select range as id from range(1200)", |n| n.batch_rows = Some(100));
    let run = run_all(&st, &job).await;
    assert_eq!(run.status, "success", "{run:?}");
    let log = api::job_run_log(&st, &job.id, run.id).unwrap();
    assert!(log.iter().any(|e| e.message.contains("INSERTs of up to 100 rows")), "{log:#?}");
    assert!(log.iter().any(|e| e.message.contains("1200 rows into batched in 12 batches")), "{log:#?}");
    assert_eq!(rows(&st, &dst, "select count(*) from batched").await, vec![vec!["1200"]]);

    // Past SQLite's 1,000,000-byte statement limit: lowered, and the dry run says so.
    let mut j = st.workspace.get_job(&job.id).unwrap();
    j.nodes[1].batch_kb = Some(4096);
    let r = api::dry_run_job_step(&st, j, "b").await.unwrap();
    assert_eq!(r.batch.unwrap().bytes, 1_000_000);
    assert!(r.notices.iter().any(|n| n.contains("SQLITE_MAX_SQL_LENGTH")), "{r:?}");
}
