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
        for mode in [LoadMode::Replace, LoadMode::Truncate] {
            let mut j = st.workspace.get_job(&job.id).unwrap();
            j.nodes[1].load_mode = mode;
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
