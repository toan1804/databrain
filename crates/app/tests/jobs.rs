//! Jobs end to end: a SQLite query step, a DuckDB step reading its output,
//! and a load step writing into another SQLite database.

use std::sync::Arc;
use std::time::Duration;

use databrain_app::api::{self, AppState, SaveConnectionArgs};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::{EventSink, JobEvent, RunRequest};
use databrain_workspace::{ConnectionProfile, EnvTag, FileFormat, Job, JobEdge, JobNode, JobNodeKind, JobRun, JobSchedule, LoadMode, Workspace};

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
            profile: ConnectionProfile {
                id: String::new(),
                name: name.into(),
                config: c,
                color: None,
                env: EnvTag::None,
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
        ai_sql: false,
        limits: Default::default(),
        last_run: None,
    }
}

/// Every test ends in time: a hang fails it (with where it hung) instead
/// of stopping the whole test run.
async fn guarded(test: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(240), test).await.expect("the test hung (more than 4 minutes)");
}

/// Poll `cond` every 20 ms for up to `secs` s; fails naming `what` if it never holds.
async fn until(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(std::time::Instant::now() < end, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The newest run of `job`: the status of step `node` in it.
fn step_status(st: &AppState, job: &str, node: &str) -> Option<String> {
    let run = st.workspace.job_runs(job, 1).ok()?.into_iter().next()?;
    run.nodes.into_iter().find(|n| n.node_id == node).map(|n| n.summary.status)
}

/// The newest run of `job` has a log line starting with `prefix`.
fn logged(st: &AppState, job: &str, prefix: &str) -> bool {
    let Some(run) = st.workspace.job_runs(job, 1).ok().and_then(|r| r.into_iter().next()) else { return false };
    api::job_run_log(st, job, run.id).unwrap_or_default().iter().any(|l| l.message.starts_with(prefix))
}

async fn finished(st: &AppState, job: &str) -> JobRun {
    // Up to a minute: a slow step (DuckDB summing 300 M rows) takes seconds
    // more on a loaded machine.
    for _ in 0..2400 {
        let run = st.workspace.job_runs(job, 1).unwrap().remove(0);
        // The run is recorded before the job lets go of its slot.
        if run.status != "running" && !st.jobs.is_running(job) {
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
        .run_and_wait(
            &st.hub,
            RunRequest {
                connection_id: conn.into(),
                tab_id: format!("t-{}", uuid::Uuid::new_v4()),
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
    if let Some(e) = out.iter().find_map(|o| o.error.as_ref()) {
        panic!("{}: {sql}", e.message);
    }
    let id = out.iter().rev().find_map(|o| o.result.clone()).expect("no result").id;
    let page = api::fetch_page(st, id, Default::default(), 0, 1000).await.unwrap();
    page.rows.into_iter().map(|r| r.into_iter().map(|c| c.unwrap_or_default()).collect()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn query_duckdb_and_load_steps() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
        rows(
            &st,
            &src,
            "create table orders (id integer, region text, amount real); insert into orders values (1, 'eu', 10.5), (2, 'eu', 4.5), (3, 'us', 7); select 1",
        )
        .await;

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
                    node(
                        "b",
                        "by_region",
                        JobNodeKind::Query,
                        None,
                        "select region, sum(amount) as total, count(*) as n from results.orders group by region order by region",
                    ),
                    load,
                ],
                edges: vec![JobEdge { from: "a".into(), to: "b".into() }, JobEdge { from: "b".into(), to: "c".into() }],
                schedule: JobSchedule { enabled: true, minutes: 60, ..Default::default() },
                last_scheduled_at: None,
                params: Default::default(),
                param_set: None,
                limits: Default::default(),
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
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_skip_downstream_and_read_only_is_refused() {
    guarded(async {
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
                params: Default::default(),
                param_set: None,
                limits: Default::default(),
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
    })
    .await;
}

/// Load step into Postgres / MySQL (skipped unless `DATABRAIN_PG_HOST` /
/// `DATABRAIN_MYSQL_HOST` are set): types, quoting, NULLs, Replace + Truncate.
#[tokio::test(flavor = "multi_thread")]
async fn load_into_server_databases() {
    guarded(async {
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
                    profile: ConnectionProfile {
                        id: String::new(),
                        name: "db".into(),
                        config: c,
                        color: None,
                        env: EnvTag::None,
                        folder_id: None,
                        has_secret: true,
                        ai_policy: Default::default(),
                        created_at: 0,
                        updated_at: 0,
                    },
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
                    params: Default::default(),
                    param_set: None,
                    limits: Default::default(),
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
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn export_steps_write_files_with_duckdb() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let mut nodes = vec![node("a", "src_rows", JobNodeKind::Query, None, "select 1 as id, 'it''s, quoted' as name union all select 2, 'b'")];
        let mut edges = vec![];
        for (i, (fmt, file)) in
            [(FileFormat::Csv, Some("report {step}")), (FileFormat::Parquet, None), (FileFormat::Json, Some("rows.json"))].into_iter().enumerate()
        {
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
            .save_job(Job {
                id: String::new(),
                name: "Export".into(),
                nodes,
                edges,
                schedule: Default::default(),
                last_scheduled_at: None,
                params: Default::default(),
                param_set: None,
                limits: Default::default(),
                created_at: 0,
                updated_at: 0,
            })
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
    })
    .await;
}

/// A step's result capped at the memory a result may keep holds part of
/// the rows: the steps that read it fail before running, so none exports
/// or loads that part (or a complete-looking copy of it) as if it were all.
#[tokio::test(flavor = "multi_thread")]
async fn capped_results_are_not_exported_or_loaded() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        st.engine.outputs().set_budget(0); // 16 MB: 8 MB per result
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
        let mut export = node("e", "big_file", JobNodeKind::Export, None, "");
        export.export_folder = Some(dir.path().to_string_lossy().into_owned());
        let mut load = node("l", "big_copy", JobNodeKind::Load, None, "");
        load.target_connection_id = Some(dst.clone());
        load.target_table = Some("big".into());
        // A query on a DuckDB file reads `results.*` too: refused the same way.
        let file = duckdb(&st, "file", &dir.path().join("file.duckdb"));
        let on_file = node("q", "big_count", JobNodeKind::Query, Some(&file), "select count(*) as n from results.big");
        let job = st
            .workspace
            .save_job(Job {
                id: String::new(),
                name: "Capped".into(),
                nodes: vec![
                    node(
                        "a",
                        "big",
                        JobNodeKind::Query,
                        Some(&src),
                        "with recursive n(i) as (select 1 union all select i + 1 from n where i < 400) select i, randomblob(100000) as b from n",
                    ),
                    export,
                    load,
                    on_file,
                ],
                edges: vec![
                    JobEdge { from: "a".into(), to: "e".into() },
                    JobEdge { from: "a".into(), to: "l".into() },
                    JobEdge { from: "a".into(), to: "q".into() },
                ],
                schedule: Default::default(),
                last_scheduled_at: None,
                params: Default::default(),
                param_set: None,
                limits: Default::default(),
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        let run = finished(&st, &job.id).await;
        let rec = |n: &str| run.nodes.iter().find(|r| r.name == n).unwrap().summary.clone();
        assert!(rec("big").error.is_none(), "a capped query step runs (it says so)");
        // The steps that read it fail before they write anything.
        for n in ["big_file", "big_copy", "big_count"] {
            let e = rec(n).error.unwrap_or_default();
            assert!(e.contains("holds only its first"), "{n}: {e}");
        }
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|f| !f.unwrap().file_name().to_string_lossy().starts_with("big_file")), "no file");
        assert_eq!(rows(&st, &dst, "select count(*) from sqlite_master where name = 'big'").await, vec![vec!["0"]], "no table");
    })
    .await;
}

/// An export never replaces a file it didn't write: the user's file is kept
/// and the rows go to `name (2)`; the job's own file is replaced next run.
#[tokio::test(flavor = "multi_thread")]
async fn export_steps_keep_files_they_did_not_write() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        std::fs::write(dir.path().join("report.csv"), "the user's file").unwrap();
        let mut e = node("e", "out", JobNodeKind::Export, None, "select 1 as id");
        e.export_folder = Some(dir.path().to_string_lossy().into_owned());
        e.export_file = Some("report".into());
        let mut job = st.workspace.save_job(plain_job("Keep", vec![e])).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        let run = finished(&st, &job.id).await;
        let rec = run.nodes[0].summary.clone();
        assert_eq!(run.status, "success", "{:?}", rec.error);
        assert_eq!(std::fs::read_to_string(dir.path().join("report.csv")).unwrap(), "the user's file");
        let file = rec.file.unwrap();
        assert!(file.ends_with("report (2).csv"), "{file}");
        assert!(rec.notices.iter().any(|n| n.contains("already exists")), "{:?}", rec.notices);
        // Next run: report (2).csv is this job's, so it is updated in place.
        job.nodes[0].sql = "select 2 as id".into();
        let job = st.workspace.save_job(job).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        let run = finished(&st, &job.id).await;
        assert_eq!(run.nodes[0].summary.file.as_deref(), Some(file.as_str()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "id\n2\n");
        let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 2, "no temporary files left: {names:?}");
    })
    .await;
}

fn plain_job(name: &str, nodes: Vec<JobNode>) -> Job {
    Job {
        id: String::new(),
        name: name.into(),
        nodes,
        edges: vec![],
        schedule: Default::default(),
        last_scheduled_at: None,
        params: Default::default(),
        param_set: None,
        limits: Default::default(),
        created_at: 0,
        updated_at: 0,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn step_names_own_their_output_names() {
    guarded(async {
        let st = state();
        let duck = st.engine.results_connection().unwrap().id;
        // A query tab already produced `revenue`: a step can't take that name.
        let out = st
            .engine
            .run_and_wait(
                &st.hub,
                RunRequest {
                    connection_id: duck.clone(),
                    tab_id: "tab1".into(),
                    sql: "select 1 as x".into(),
                    base_offset: 0,
                    row_limit: None,
                    confirmed: true,
                    origin: Default::default(),
                    session_key: None,
                    output_name: Some("revenue".into()),
                    params: Default::default(),
                    output_masks: None,
                },
                None,
            )
            .await
            .unwrap();
        assert!(out[0].error.is_none());
        let err = api::save_job(&st, plain_job("A", vec![node("a", "Revenue", JobNodeKind::Query, None, "select 1")])).unwrap_err().message;
        assert!(err.contains("query tab"), "{err}");

        // A step owns `sales`: other jobs, tabs and renames can't use it.
        let job = api::save_job(&st, plain_job("Nightly", vec![node("a", "sales", JobNodeKind::Query, None, "select 2 as y")])).unwrap();
        let err = api::save_job(&st, plain_job("B", vec![node("b", "SALES", JobNodeKind::Query, None, "select 1")])).unwrap_err().message;
        assert!(err.contains("job \"Nightly\""), "{err}");
        let run = |tab: &str| RunRequest {
            connection_id: duck.clone(),
            tab_id: tab.into(),
            sql: "select 3".into(),
            base_offset: 0,
            row_limit: None,
            confirmed: true,
            origin: Default::default(),
            session_key: None,
            output_name: Some("sales".into()),
            params: Default::default(),
            output_masks: None,
        };
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
    })
    .await;
}

fn duckdb(st: &AppState, name: &str, path: &std::path::Path) -> String {
    let mut c = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
    c.file_path = Some(path.to_string_lossy().into_owned());
    api::save_connection(
        st,
        SaveConnectionArgs {
            profile: ConnectionProfile {
                id: String::new(),
                name: name.into(),
                config: c,
                color: None,
                env: EnvTag::None,
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
            params: Default::default(),
            param_set: None,
            limits: Default::default(),
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
    guarded(async {
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
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_load_is_rolled_back() {
    guarded(async {
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
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn update_and_merge_by_key_columns() {
    guarded(async {
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
        assert_eq!(rows(&st, &dst, "select id, name, qty, note from items order by id").await, vec![vec!["1", "a", "1", "n1"], vec!["2", "B", "20", "n2"]]);
        // Merge: id 2 updated again, id 3 inserted.
        let job = load_job(&st, &dst, "items", "select 2 as id, 'bb' as name, 22 as qty union all select 3, 'c', 30", |n| {
            n.load_mode = LoadMode::Merge;
            n.key_columns = vec!["id".into()];
        });
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{dst}: {run:?}");
        assert_eq!(step(&run, "loaded").rows, Some(2));
        assert_eq!(
            rows(&st, &dst, "select id, name, qty from items order by id").await,
            vec![vec!["1", "a", "1"], vec!["2", "bb", "22"], vec!["3", "c", "30"]]
        );

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
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_checks_without_keeping_changes() {
    guarded(async {
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
    })
    .await;
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
    guarded(async {
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
            .filter_map(|(_, p)| {
                p["run"]["nodes"]
                    .as_array()?
                    .iter()
                    .find(|n| n["name"] == "loaded")?
                    .get("progress")
                    .map(|g| (g["done"].as_u64().unwrap(), g["total"].as_u64().unwrap()))
            })
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
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_per_insert_batch_is_configurable() {
    guarded(async {
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
    })
    .await;
}

/// Stop ends a running job at once: the step's statement is cancelled, the
/// step and the run are marked cancelled. The same for a single step run
/// ("Run this step"), which runs as the job's run too.
#[tokio::test(flavor = "multi_thread")]
async fn stop_cancels_the_running_step() {
    guarded(async {
        let st = state();
        let dir = tempfile::tempdir().unwrap();
        let a = sqlite(&st, "A", &dir.path().join("a.db"), false);
        // About a minute of work if nothing stops it.
        let slow = "with recursive c(i) as (select 1 union all select i + 1 from c where i < 300000000) select count(*) from c";
        let job = Job {
            id: String::new(),
            name: "Slow".into(),
            nodes: vec![node("n1", "slow_step", JobNodeKind::Query, Some(&a), slow), node("n2", "after_slow", JobNodeKind::Query, Some(&a), "select 1")],
            edges: vec![JobEdge { from: "n1".into(), to: "n2".into() }],
            schedule: JobSchedule::default(),
            created_at: 0,
            updated_at: 0,
            last_scheduled_at: None,
            params: Default::default(),
            param_set: None,
            limits: Default::default(),
        };
        let job = st.workspace.save_job(job).unwrap();
        for only in [None, Some(vec!["n1".to_string()])] {
            api::run_job(&st, &job.id, only.clone()).unwrap();
            until("the slow step running", 30, || step_status(&st, &job.id, "n1").as_deref() == Some("running")).await;
            let t = std::time::Instant::now();
            assert!(api::cancel_job(&st, &job.id), "a run to stop ({only:?})");
            let run = finished(&st, &job.id).await;
            // About a minute if Stop didn't reach the statement.
            assert!(t.elapsed() < Duration::from_secs(20), "stopped in {:?} ({only:?})", t.elapsed());
            assert_eq!(run.status, "cancelled", "{run:?}");
            let s = run.nodes.iter().find(|n| n.node_id == "n1").unwrap();
            assert_eq!(s.summary.status, "cancelled", "{run:?}");
            if only.is_none() {
                assert_eq!(run.nodes.iter().find(|n| n.node_id == "n2").map(|n| n.summary.status.as_str()), Some("cancelled"), "{run:?}");
            }
            assert!(!st.jobs.is_running(&job.id));
        }
    })
    .await;
}

/// Stopping one running step: it is cancelled, the steps after it don't run.
#[tokio::test(flavor = "multi_thread")]
async fn stop_one_step() {
    guarded(async {
        let st = state();
        let dir = tempfile::tempdir().unwrap();
        let a = sqlite(&st, "A", &dir.path().join("a.db"), false);
        let slow = "with recursive c(i) as (select 1 union all select i + 1 from c where i < 300000000) select count(*) from c";
        let job = st
            .workspace
            .save_job(Job {
                id: String::new(),
                name: "Two".into(),
                nodes: vec![node("n1", "slow_one", JobNodeKind::Query, Some(&a), slow), node("n2", "quick_one", JobNodeKind::Query, Some(&a), "select 1")],
                edges: vec![],
                schedule: JobSchedule::default(),
                created_at: 0,
                updated_at: 0,
                last_scheduled_at: None,
                params: Default::default(),
                param_set: None,
                limits: Default::default(),
            })
            .unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        until("the slow step running", 30, || step_status(&st, &job.id, "n1").as_deref() == Some("running")).await;
        assert!(api::cancel_job_step(&st, &job.id, "n1"));
        let run = finished(&st, &job.id).await;
        assert_eq!(step(&run, "slow_one").status, "cancelled", "{run:?}");
        assert_eq!(step(&run, "quick_one").status, "success", "{run:?}");
        assert!(!api::cancel_job_step(&st, &job.id, "n1"), "nothing left to stop");
    })
    .await;
}

/// A step on a connection that can't connect fails quickly, every time.
#[tokio::test(flavor = "multi_thread")]
async fn unreachable_connection_fails_each_run() {
    guarded(async {
        let st = state();
        let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
        c.file_path = Some("/nonexistent-dir/x.db".into());
        c.read_only = true;
        let bad = api::save_connection(
            &st,
            SaveConnectionArgs {
                profile: ConnectionProfile {
                    id: String::new(),
                    name: "Bad".into(),
                    config: c,
                    color: None,
                    env: EnvTag::None,
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
        .unwrap()
        .id;
        let job = st
            .workspace
            .save_job(Job {
                id: String::new(),
                name: "Bad".into(),
                nodes: vec![node("n1", "bad_one", JobNodeKind::Query, Some(&bad), "select 1")],
                edges: vec![],
                schedule: JobSchedule::default(),
                created_at: 0,
                updated_at: 0,
                last_scheduled_at: None,
                params: Default::default(),
                param_set: None,
                limits: Default::default(),
            })
            .unwrap();
        for i in 0..8 {
            let mut tries = 0;
            let r = loop {
                match api::run_job(&st, &job.id, Some(vec!["n1".into()])) {
                    Ok(r) => break r,
                    Err(e) if e.kind == "busy" && tries < 500 => {
                        tries += 1;
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(e) => panic!("run {i}: {e:?}"),
                }
            };
            let run = finished(&st, &job.id).await;
            assert_eq!((run.id, run.status.as_str()), (r.id, "error"), "run {i}: {run:?}");
            assert_eq!(step(&run, "bad_one").status, "error");
        }
    })
    .await;
}

/// The app and the background runner (two processes on one workspace): a due
/// job runs once, a job running in one can't start in the other, and the
/// app doesn't cancel the runner's live runs.
#[tokio::test(flavor = "multi_thread")]
async fn app_and_background_runner_share_jobs_safely() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.db");
        let open = |kind: &str| {
            let mut st = AppState::new(Arc::new(Workspace::open(&path).unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
            st.jobs = Arc::new(databrain_app::jobs::JobsState::new(kind));
            st
        };
        let (app, cli) = (open("app"), open("cli"));
        let conn = sqlite(&app, "src", &dir.path().join("src.db"), false);
        let mut job = plain_job("Nightly", vec![node("a", "nightly_rows", JobNodeKind::Query, Some(&conn), "select 1 as x")]);
        job.schedule = JobSchedule { enabled: true, minutes: 5, ..Default::default() };
        let job = app.workspace.save_job(job).unwrap();
        app.workspace.backdate_job_for_tests(&job.id, databrain_workspace::now_ms() - 3_600_000).unwrap();

        // Both look for due jobs at the same moment (two threads let go
        // together): only one starts it.
        let gate = Arc::new(std::sync::Barrier::new(2));
        let (actx, cctx) = (app.job_ctx(), cli.job_ctx());
        let (g1, g2) = (gate.clone(), gate.clone());
        let a = tokio::task::spawn_blocking(move || {
            g1.wait();
            databrain_app::jobs::tick(&actx)
        });
        let b = tokio::task::spawn_blocking(move || {
            g2.wait();
            databrain_app::jobs::tick_as(&cctx, "background")
        });
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert_eq!(a.len() + b.len(), 1, "app {a:?}, runner {b:?}");
        let ran_in = if a.is_empty() { &cli } else { &app };
        let run = finished(ran_in, &job.id).await;
        assert_eq!(run.status, "success", "{run:?}");
        // Once the lease is let go, nothing is due any more.
        until("the lease let go", 10, || app.workspace.job_lease(&job.id).unwrap().is_none()).await;
        assert!(databrain_app::jobs::tick(&app.job_ctx()).is_empty() && databrain_app::jobs::tick_as(&cli.job_ctx(), "background").is_empty());
        assert_eq!(app.workspace.job_runs(&job.id, 10).unwrap().len(), 1);

        // Running in the runner: the app refuses to start it and lists it as running elsewhere.
        assert!(cli.workspace.claim_job(&job.id, &cli.jobs.owner, databrain_workspace::now_ms() + 60_000).unwrap());
        let e = api::run_job(&app, &job.id, None).unwrap_err();
        assert!(e.message.contains("background runner"), "{}", e.message);
        assert_eq!(api::list_jobs(&app).unwrap().elsewhere, vec![job.id.clone()]);
        cli.workspace.release_job(&job.id, &cli.jobs.owner).unwrap();

        // A run of the command line is not closed by the app's stale-run check.
        let r = databrain_app::jobs::run_and_wait(&cli.job_ctx(), &job.id, "cli").await.unwrap();
        assert_eq!((r.status.as_str(), r.trigger.as_str()), ("success", "cli"));
        let live = cli.workspace.start_job_run(&job.id, "background").unwrap();
        cli.workspace.touch_job_run(live.id, &cli.jobs.owner).unwrap();
        assert_eq!(app.workspace.close_stale_job_runs().unwrap(), 0);
    })
    .await;
}

/// Job parameters: the steps' `:name` values come from the job's set, then
/// its own values, then the run's; the log names them and the run records
/// them; a resume runs with the same; a deleted set fails the run's steps.
#[tokio::test(flavor = "multi_thread")]
async fn job_steps_get_parameter_values() {
    guarded(async {
        use databrain_app::jobs::RunParams;
        use databrain_connector_core::sql::ParamValue;
        let st = state();
        let v = |s: &str| ParamValue { value: s.into(), raw: false };
        let daily = st
            .workspace
            .save_param_set(databrain_workspace::ParamSet {
                id: String::new(),
                name: "Daily".into(),
                values: [("day".to_string(), v("2026-11-01")), ("tenant".to_string(), v("acme"))].into(),
                updated_at: 0,
            })
            .unwrap();
        let mut job = Job {
            id: String::new(),
            name: "Params".into(),
            nodes: vec![node("p", "picked", JobNodeKind::Query, None, "select :day as day, :tenant as tenant, cast(:n as integer) as n")],
            edges: vec![],
            schedule: JobSchedule::default(),
            last_scheduled_at: None,
            params: [("tenant".to_string(), v("globex")), ("n".to_string(), v("5"))].into(),
            param_set: Some(daily.id.clone()),
            limits: Default::default(),
            created_at: 0,
            updated_at: 0,
        };
        job = st.workspace.save_job(job).unwrap();
        let duck = st.engine.results_connection().unwrap().id;

        api::run_job(&st, &job.id, None).unwrap();
        let run = finished(&st, &job.id).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(
            rows(&st, &duck, "select day, tenant, n from results.picked").await,
            vec![vec!["2026-11-01", "globex", "5"]],
            "the job's own values over its set's"
        );
        assert_eq!(run.params["tenant"].value, "globex");
        let log = st.workspace.job_run_log(&job.id, run.id).unwrap();
        assert!(log.iter().any(|l| l.message.contains("Parameters (set \"Daily\")") && l.message.contains(":day = 2026-11-01")), "{log:?}");

        // This run only: another value over the job's.
        let over = RunParams { set: None, values: [("day".to_string(), v("2026-12-24"))].into() };
        api::run_job_with(&st, &job.id, None, Some(over)).unwrap();
        assert_eq!(finished(&st, &job.id).await.status, "success");
        assert_eq!(rows(&st, &duck, "select day, tenant, n from results.picked").await, vec![vec!["2026-12-24", "globex", "5"]]);
        // A value that fails the step; Resume runs again with the same values.
        let bad = RunParams { set: None, values: [("n".to_string(), v("oops"))].into() };
        api::run_job_with(&st, &job.id, None, Some(bad)).unwrap();
        assert_eq!(finished(&st, &job.id).await.status, "error");
        api::resume_job(&st, &job.id).unwrap();
        let resumed = finished(&st, &job.id).await;
        assert_eq!((resumed.trigger.as_str(), resumed.status.as_str(), resumed.params["n"].value.as_str()), ("resume", "error", "oops"), "{resumed:?}");

        // The set is gone: the step fails and says so.
        st.workspace.delete_param_set(&daily.id).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        let run = finished(&st, &job.id).await;
        assert_eq!(run.status, "error");
        assert!(run.nodes[0].summary.error.as_deref().is_some_and(|e| e.contains("parameter set")), "{run:?}");

        // No set and no value for :day: the step says which one is missing.
        job.param_set = None;
        job.params.remove("tenant");
        st.workspace.save_job(job.clone()).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        let run = finished(&st, &job.id).await;
        assert!(run.nodes[0].summary.error.as_deref().is_some_and(|e| e.contains(":day") && e.contains(":tenant")), "{run:?}");
    })
    .await;
}

/// The lease ran out while the first step ran (the computer slept) and
/// another runner took the job: this run stops before its next step, so
/// the job doesn't run twice.
#[tokio::test(flavor = "multi_thread")]
async fn lost_lease_stops_the_run() {
    guarded(async {
        let st = state();
        let mut job = Job {
            id: String::new(),
            name: "Lease".into(),
            nodes: vec![
                node("a", "slow", JobNodeKind::Query, None, "select sum(hash(i)) as h from range(300000000) t(i)"),
                node("b", "after", JobNodeKind::Query, None, "select 1 as x"),
            ],
            edges: vec![JobEdge { from: "a".into(), to: "b".into() }],
            schedule: JobSchedule::default(),
            last_scheduled_at: None,
            params: Default::default(),
            param_set: None,
            limits: Default::default(),
            created_at: 0,
            updated_at: 0,
        };
        job = st.workspace.save_job(job).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        // Taken while the first step runs (if it ends first, the check before
        // the next step finds the lease gone all the same).
        until("the first step running", 30, || step_status(&st, &job.id, "a").as_deref() == Some("running")).await;
        st.workspace.expire_job_lease_for_tests(&job.id).unwrap();
        assert!(st.workspace.claim_job(&job.id, "cli:other", databrain_workspace::now_ms() + 90_000).unwrap(), "the other runner takes it");
        let run = finished(&st, &job.id).await;
        assert_eq!(run.status, "cancelled", "{run:?}");
        let b = run.nodes.iter().find(|n| n.node_id == "b").unwrap();
        assert_eq!(b.summary.status, "cancelled", "the next step didn't start: {run:?}");
        let log = st.workspace.job_run_log(&job.id, run.id).unwrap();
        assert!(log.iter().any(|l| l.message.contains("another runner took this job")), "{log:?}");
        assert_eq!(st.workspace.job_lease(&job.id).unwrap().map(|(o, _)| o).as_deref(), Some("cli:other"), "its lease isn't released by this run");
    })
    .await;
}

/// A scheduled job whose start fails keeps its slot: it is still due on
/// the next tick, instead of waiting for the next period.
#[tokio::test(flavor = "multi_thread")]
async fn failed_scheduled_start_keeps_its_slot() {
    let st = state();
    let mut job = plain_job("Empty", vec![]);
    job.schedule = JobSchedule { enabled: true, minutes: 60, ..Default::default() };
    let job = st.workspace.save_job(job).unwrap();
    let before = databrain_workspace::now_ms() - 7_200_000;
    st.workspace.backdate_job_for_tests(&job.id, before).unwrap();
    assert!(databrain_app::jobs::tick(&st.job_ctx()).is_empty(), "a job without steps doesn't start");
    let after = st.workspace.get_job(&job.id).unwrap();
    assert_eq!(after.last_scheduled_at, Some(before), "the slot is given back");
    assert!(
        st.workspace.list_jobs().unwrap().iter().find(|j| j.id == job.id).unwrap().next_run_at.is_some_and(|t| t <= databrain_workspace::now_ms()),
        "still due"
    );
    assert!(st.workspace.job_lease(&job.id).unwrap().is_none(), "the lease is released");
}
/// A step whose SQL the assistant wrote is checked against the connection's
/// AI settings each time it runs, not only when the job was approved: with
/// AI turned off for the connection it stops, and says why. The user's own
/// step on the same connection is not the assistant's and keeps running.
#[tokio::test(flavor = "multi_thread")]
async fn steps_the_assistant_wrote_follow_todays_ai_settings() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let mut written_by_ai = node("a", "ai_step", JobNodeKind::Query, Some(&src), "select 1 as one");
        written_by_ai.ai_sql = true;
        let mine = node("b", "my_step", JobNodeKind::Query, Some(&src), "select 2 as two");
        let job = st.workspace.save_job(plain_job("Mixed", vec![written_by_ai, mine])).unwrap();
        assert!(job.nodes[0].ai_sql && !job.nodes[1].ai_sql);
        // Allowed: AI is on for the connection (the default) and the step reads.
        let run = run_all(&st, &job).await;
        assert_eq!((run.status.as_str(), step(&run, "ai_step").status.as_str(), step(&run, "my_step").status.as_str()), ("success", "success", "success"));
        // Refused: AI is turned off for the connection afterwards.
        let mut p = st.workspace.get_connection(&src).unwrap();
        p.ai_policy.ai_enabled = false;
        st.workspace.save_connection(p).unwrap();
        let run = run_all(&st, &job).await;
        let ai = step(&run, "ai_step");
        assert_eq!(ai.status, "error");
        let why = ai.error.unwrap_or_default();
        assert!(why.contains("AI settings no longer allow") && why.contains("AI is disabled"), "{why}");
        assert_eq!(step(&run, "my_step").status, "success", "the user's own step is not the assistant's");
        // And a write the assistant wrote, once "AI may write" is off again.
        let mut p = st.workspace.get_connection(&src).unwrap();
        p.ai_policy.ai_enabled = true;
        p.ai_policy.allow_write = true;
        st.workspace.save_connection(p.clone()).unwrap();
        let mut write = node("a", "ai_write", JobNodeKind::Query, Some(&src), "create table if not exists made_by_ai (x int)");
        write.ai_sql = true;
        let job = st.workspace.save_job(plain_job("Writes", vec![write])).unwrap();
        assert_eq!(run_all(&st, &job).await.status, "success");
        p.ai_policy.allow_write = false;
        st.workspace.save_connection(p).unwrap();
        let run = run_all(&st, &job).await;
        let why = step(&run, "ai_write").error.unwrap_or_default();
        assert!(run.status == "error" && why.contains("not allowed for the AI"), "{why}");
    })
    .await;
}

/// A job of `nodes` with `limits` and these links, saved.
fn limited_job(st: &AppState, name: &str, nodes: Vec<JobNode>, edges: Vec<(&str, &str)>, limits: databrain_workspace::RunLimits) -> Job {
    let mut job = plain_job(name, nodes);
    job.edges = edges.into_iter().map(|(a, b)| JobEdge { from: a.into(), to: b.into() }).collect();
    job.limits = limits;
    st.workspace.save_job(job).unwrap()
}

/// About a minute of SQLite work if nothing stops it.
const SLOW: &str = "with recursive c(i) as (select 1 union all select i + 1 from c where i < 300000000) select count(*) from c";

#[tokio::test(flavor = "multi_thread")]
async fn time_limits_stop_steps_and_runs() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let st = state();
        let dir = tempfile::tempdir().unwrap();
        let a = sqlite(&st, "A", &dir.path().join("a.db"), false);

        // A step past its own limit fails (the others go on), quickly.
        let mut slow = node("n1", "slow_limited", JobNodeKind::Query, Some(&a), SLOW);
        slow.limits.timeout_secs = Some(1);
        let quick = node("n2", "quick_limited", JobNodeKind::Query, Some(&a), "select 1");
        let job = limited_job(&st, "Step limit", vec![slow, quick], vec![], Default::default());
        let t = std::time::Instant::now();
        let run = run_all(&st, &job).await;
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        assert_eq!(run.status, "error", "{run:?}");
        let s = step(&run, "slow_limited");
        assert_eq!(s.status, "error");
        assert!(s.error.as_deref().unwrap().contains("Timed out after 1 s"), "{s:?}");
        assert_eq!(step(&run, "quick_limited").status, "success");
        let log = api::job_run_log(&st, &job.id, run.id).unwrap();
        assert!(log.iter().any(|l| l.message.starts_with("Time limit reached (1 s)")), "{log:?}");

        // The job's step limit applies to steps without their own; 0 turns it off.
        let own = node("n1", "slow_default", JobNodeKind::Query, Some(&a), SLOW);
        let mut off = node("n2", "quick_off", JobNodeKind::Query, Some(&a), "select 1");
        off.limits.timeout_secs = Some(0);
        let job2 =
            limited_job(&st, "Job step limit", vec![own, off], vec![], databrain_workspace::RunLimits { step_timeout_secs: Some(1), ..Default::default() });
        let run = run_all(&st, &job2).await;
        assert!(step(&run, "slow_default").error.as_deref().unwrap().contains("Timed out"), "{run:?}");
        assert_eq!(step(&run, "quick_off").status, "success");

        // A run past its limit is stopped and fails: the running step and those after it are stopped.
        let first = node("n1", "slow_run", JobNodeKind::Query, Some(&a), SLOW);
        let after = node("n2", "after_run", JobNodeKind::Query, Some(&a), "select 1");
        let job3 = limited_job(
            &st,
            "Run limit",
            vec![first, after],
            vec![("n1", "n2")],
            databrain_workspace::RunLimits { run_timeout_secs: Some(1), ..Default::default() },
        );
        let t = std::time::Instant::now();
        let run = run_all(&st, &job3).await;
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        assert_eq!(run.status, "error", "{run:?}");
        assert!(run.error.as_deref().unwrap().contains("timed out after 1 s"), "{run:?}");
        assert_eq!(step(&run, "slow_run").status, "cancelled");
        assert_eq!(step(&run, "after_run").status, "cancelled");
        assert!(!st.jobs.is_running(&job3.id));
        // The job is free again (its lease let go).
        assert!(st.workspace.claim_job(&job3.id, "someone-else", databrain_workspace::now_ms() + 1000).unwrap());
    })
    .await
    .expect("test hung");
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_errors_are_tried_again_only_when_safe() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let st = state();
        // The database appears during the wait before the retry (the first try
        // fails within the first second; the retry comes at 3 s).
        st.jobs.set_retry_delay(Duration::from_secs(3));
        let dir = tempfile::tempdir().unwrap();
        // A folder that isn't there yet: the database can't be opened.
        let folder = dir.path().join("later");
        let path = folder.join("later.db");
        let later = sqlite(&st, "Later", &path, false);

        // A read: tried again once the database is there.
        let mut read = node("n1", "read_later", JobNodeKind::Query, Some(&later), "select 42");
        read.limits.retries = Some(3);
        let job = limited_job(&st, "Retry", vec![read], vec![], Default::default());
        api::run_job(&st, &job.id, None).unwrap();
        // The database appears once the first try has failed (3 s before the retry).
        until("the first try failed", 30, || logged(&st, &job.id, "Try 1 of 4 failed")).await;
        std::fs::create_dir(&folder).unwrap();
        let run = finished(&st, &job.id).await;
        assert_eq!(run.status, "success", "{run:?}");
        let s = step(&run, "read_later");
        assert_eq!((s.attempt, s.attempts), (Some(2), Some(4)), "{s:?}");
        let log = api::job_run_log(&st, &job.id, run.id).unwrap();
        assert!(log.iter().any(|l| l.message.starts_with("Try 1 of 4 failed") && l.message.contains("trying again")), "{log:?}");

        // A write is never run again: it may have been kept.
        std::fs::remove_dir_all(&folder).unwrap();
        let mut write = node("n1", "write_later", JobNodeKind::Query, Some(&later), "create table t (x integer)");
        write.limits.retries = Some(3);
        let job2 = limited_job(&st, "No retry", vec![write], vec![], Default::default());
        let run = run_all(&st, &job2).await;
        assert_eq!(run.status, "error", "{run:?}");
        assert_eq!(step(&run, "write_later").attempt, Some(1));
        let log = api::job_run_log(&st, &job2.id, run.id).unwrap();
        assert!(log.iter().any(|l| l.message.starts_with("Not tried again: its SQL may write")), "{log:?}");
        assert!(!log.iter().any(|l| l.message.contains("trying again")), "{log:?}");

        // An error in the SQL isn't a connection error: not tried again either.
        std::fs::create_dir(&folder).unwrap();
        let mut bad = node("n1", "bad_sql", JobNodeKind::Query, Some(&later), "select * from missing_table");
        bad.limits.retries = Some(3);
        let job3 = limited_job(&st, "SQL error", vec![bad], vec![], Default::default());
        let run = run_all(&st, &job3).await;
        assert_eq!((run.status.as_str(), step(&run, "bad_sql").attempt), ("error", Some(1)), "{run:?}");
    })
    .await
    .expect("test hung");
}

/// (start, end) of each step, from its log.
fn spans(log: &[databrain_workspace::RunLogEntry]) -> Vec<(i64, i64)> {
    let mut by: std::collections::HashMap<String, (i64, i64)> = Default::default();
    for l in log {
        let Some(s) = &l.step else { continue };
        if l.message.starts_with("Started") {
            by.entry(s.clone()).or_default().0 = l.at;
        } else if l.message.starts_with("Succeeded") {
            by.entry(s.clone()).or_default().1 = l.at;
        }
    }
    by.into_values().collect()
}

/// Most steps running at the same time.
fn most_at_once(spans: &[(i64, i64)]) -> usize {
    spans.iter().map(|(s, _)| spans.iter().filter(|(a, b)| a <= s && s < b).count()).max().unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn steps_at_once_stay_within_the_limits() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let st = state();
        let dir = tempfile::tempdir().unwrap();
        let work = "with recursive c(i) as (select 1 union all select i + 1 from c where i < 1500000) select count(*) from c";

        // One connection, at most one step on it at once.
        let a = sqlite(&st, "A", &dir.path().join("a.db"), false);
        let nodes = (1..=3).map(|i| node(&format!("n{i}"), &format!("same_conn_{i}"), JobNodeKind::Query, Some(&a), work)).collect();
        let job = limited_job(&st, "Per connection", nodes, vec![], databrain_workspace::RunLimits { max_per_connection: Some(1), ..Default::default() });
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        let log = api::job_run_log(&st, &job.id, run.id).unwrap();
        let s = spans(&log);
        assert_eq!(s.len(), 3);
        assert_eq!(most_at_once(&s), 1, "{s:?}");
        assert!(log.iter().any(|l| l.message.starts_with("Waiting: 1 steps are running on A")), "{log:?}");

        // Three connections, at most two steps at once in the job.
        let conns: Vec<String> = (1..=3).map(|i| sqlite(&st, &format!("C{i}"), &dir.path().join(format!("c{i}.db")), false)).collect();
        let nodes = conns.iter().enumerate().map(|(i, c)| node(&format!("n{i}"), &format!("other_conn_{i}"), JobNodeKind::Query, Some(c), work)).collect();
        let job2 = limited_job(&st, "Per job", nodes, vec![], databrain_workspace::RunLimits { max_parallel: Some(2), ..Default::default() });
        let run = run_all(&st, &job2).await;
        assert_eq!(run.status, "success", "{run:?}");
        let s = spans(&api::job_run_log(&st, &job2.id, run.id).unwrap());
        assert_eq!(s.len(), 3);
        // Two at once: the limit is used, not only kept.
        assert_eq!(most_at_once(&s), 2, "{s:?}");
    })
    .await
    .expect("test hung");
}

/// A step waiting for a slot can be stopped on its own: it never starts,
/// the steps after it are cancelled, the running one goes on.
#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_step_can_be_stopped() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let st = state();
        let dir = tempfile::tempdir().unwrap();
        let a = sqlite(&st, "A", &dir.path().join("a.db"), false);
        let slow = "with recursive c(i) as (select 1 union all select i + 1 from c where i < 300000000) select count(*) from c";
        let mut first = node("n1", "runs_first", JobNodeKind::Query, Some(&a), slow);
        first.limits.timeout_secs = Some(3);
        let waits = node("n2", "waits_for_slot", JobNodeKind::Query, Some(&a), "select 1");
        let after = node("n3", "after_waiting", JobNodeKind::Query, Some(&a), "select 2");
        let mut job = plain_job("Stop waiting", vec![first, waits, after]);
        job.edges = vec![JobEdge { from: "n2".into(), to: "n3".into() }];
        job.limits.max_parallel = Some(1);
        let job = st.workspace.save_job(job).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        // n2 waits for n1's slot (n1 runs into its 3 s limit).
        until("n2 waiting for a slot", 10, || logged(&st, &job.id, "Waiting:")).await;
        assert_eq!(step_status(&st, &job.id, "n1").as_deref(), Some("running"), "stopped while n1 still runs");
        assert!(api::cancel_job_step(&st, &job.id, "n2"), "a waiting step can be stopped");
        let run = finished(&st, &job.id).await;
        assert_eq!(step(&run, "waits_for_slot").status, "cancelled", "{run:?}");
        assert_eq!(step(&run, "after_waiting").status, "cancelled", "{run:?}");
        let log = api::job_run_log(&st, &job.id, run.id).unwrap();
        assert!(log.iter().any(|l| l.message == "Not started: stopped by the user while it waited"), "{log:?}");
        assert!(!log.iter().any(|l| l.step.as_deref() == Some("waits_for_slot") && l.message.starts_with("Started")), "{log:?}");
        // The running step went on to its own time limit.
        assert!(step(&run, "runs_first").error.unwrap_or_default().contains("Timed out"), "{run:?}");
    })
    .await
    .expect("in time");
}

/// Rows the assistant's SQL selected stay the assistant's down the job: a
/// load the user wrote that takes them (here by default, from its upstream)
/// runs under the AI rules, so it is refused where the assistant may not
/// write; the same load of the user's own rows runs as before, and with
/// "AI may write" on the target it runs.
/// A query step on a DuckDB file (not Results) reads `results.*` too: one
/// that reads the assistant's rows and writes them runs under the AI rules.
#[tokio::test(flavor = "multi_thread")]
async fn duckdb_files_reading_the_assistants_rows_follow_the_ai_rules() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let file = duckdb(&st, "file", &dir.path().join("file.duckdb"));
        let job_of = |name: &str, ai: bool| {
            let mut pick = node("a", &format!("{name}_rows"), JobNodeKind::Query, Some(&src), "select 1 as id union all select 2");
            pick.ai_sql = ai;
            let copy =
                node("b", &format!("{name}_copy"), JobNodeKind::Query, Some(&file), &format!("create table {name}_t as select * from results.{name}_rows"));
            let mut j = plain_job(name, vec![pick, copy]);
            j.edges = vec![JobEdge { from: "a".into(), to: "b".into() }];
            st.workspace.save_job(j).unwrap()
        };
        // Refused: the user's step on a DuckDB file writes the assistant's rows there.
        let ai_job = job_of("dk_ai", true);
        let run = run_all(&st, &ai_job).await;
        let copy = step(&run, "dk_ai_copy");
        assert_eq!(copy.status, "error", "{run:?}");
        let log = api::job_run_log(&st, &ai_job.id, run.id).unwrap();
        assert!(
            log.iter().any(|l| l.message.starts_with("Runs under the AI rules: it reads rows the assistant's SQL selected (results.dk_ai_rows)")),
            "{log:?}"
        );
        assert_eq!(rows(&st, &file, "select count(*) from information_schema.tables where table_name = 'dk_ai_t'").await, vec![vec!["0"]], "nothing written");

        // Allowed: the same step reading the user's own rows.
        let mine = job_of("dk_mine", false);
        let run = run_all(&st, &mine).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(rows(&st, &file, "select count(*) from dk_mine_t").await, vec![vec!["2"]]);
    })
    .await
    .expect("test hung");
}

#[tokio::test(flavor = "multi_thread")]
async fn loads_of_the_assistants_rows_follow_the_ai_rules() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
        let job_of = |name: &str, ai: bool| {
            let mut pick = node("a", &format!("{name}_rows"), JobNodeKind::Query, Some(&src), "select 1 as id union all select 2");
            pick.ai_sql = ai;
            let mut load = node("b", &format!("{name}_load"), JobNodeKind::Load, None, "");
            load.target_connection_id = Some(dst.clone());
            load.target_table = Some(format!("{name}_t"));
            let mut j = plain_job(name, vec![pick, load]);
            j.edges = vec![JobEdge { from: "a".into(), to: "b".into() }];
            st.workspace.save_job(j).unwrap()
        };
        // Refused: the user's load takes the assistant's rows into a
        // connection the assistant may not write to.
        let ai_job = job_of("from_ai", true);
        assert!(!ai_job.nodes[1].ai_sql, "the load is the user's");
        let run = run_all(&st, &ai_job).await;
        assert_eq!(step(&run, "from_ai_rows").status, "success", "{run:?}");
        let load = step(&run, "from_ai_load");
        assert_eq!(load.status, "error", "{run:?}");
        assert!(load.error.as_deref().unwrap_or_default().contains("AI settings no longer allow"), "{load:?}");
        let log = api::job_run_log(&st, &ai_job.id, run.id).unwrap();
        assert!(
            log.iter().any(|l| l.message.starts_with("Runs under the AI rules: it reads rows the assistant's SQL selected (results.from_ai_rows)")),
            "{log:?}"
        );
        assert_eq!(rows(&st, &dst, "select count(*) from sqlite_master where name = 'from_ai_t'").await, vec![vec!["0"]], "nothing written");

        // Allowed: the same load of the user's own rows.
        let mine = job_of("mine", false);
        let run = run_all(&st, &mine).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(rows(&st, &dst, "select count(*) from mine_t").await, vec![vec!["2"]]);

        // Allowed: the assistant may write there.
        let mut p = st.workspace.get_connection(&dst).unwrap();
        p.ai_policy.allow_write = true;
        st.workspace.save_connection(p).unwrap();
        let run = run_all(&st, &ai_job).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(rows(&st, &dst, "select count(*) from from_ai_t").await, vec![vec!["2"]]);
    })
    .await
    .expect("test hung");
}
