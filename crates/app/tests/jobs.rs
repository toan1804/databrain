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
        load_insert_only: false,
        export_folder: None,
        export_file: None,
        export_format: FileFormat::Csv,
        ai_sql: false,
        limits: Default::default(),
        incremental_column: None,
        incremental_start: None,
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
        assert!(log.iter().any(|e| e.message.contains("batches of up to 100 rows")), "{log:#?}");
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

/// A step whose session something still holds (an earlier try left
/// behind: it didn't stop when asked) fails saying so, not "a query is
/// already running in this tab"; once that ends, the step runs.
#[tokio::test(flavor = "multi_thread")]
async fn step_still_held_by_a_try_left_behind_fails_clearly() {
    guarded(async {
        let st = state();
        let dir = tempfile::tempdir().unwrap();
        let a = sqlite(&st, "A", &dir.path().join("a.db"), false);
        let job = limited_job(&st, "Held", vec![node("n1", "held_step", JobNodeKind::Query, Some(&a), "select 1")], vec![], Default::default());
        // What a try left behind looks like: a statement still running on the step's tab.
        let tab = databrain_workspace::SessionRole::JobStep { job: job.id.clone(), node: "n1".into() }.key();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (engine, hub, (conn, key, stop)) = (st.engine.clone(), st.hub.clone(), (a.clone(), tab.clone(), cancel.clone()));
        let old = tokio::spawn(async move {
            let req = RunRequest {
                connection_id: conn,
                tab_id: key,
                sql: SLOW.into(),
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin: Default::default(),
                session_key: None,
                output_name: None,
                params: Default::default(),
                output_masks: None,
            };
            let _ = engine.run_and_wait(&hub, req, Some(stop)).await;
        });
        until("the old try running", 30, || st.engine.in_use(&tab)).await;
        let run = run_all(&st, &job).await;
        let s = step(&run, "held_step");
        assert_eq!(s.status, "error", "{run:?}");
        assert!(s.error.as_deref().unwrap().contains("earlier try of this step is still running"), "{s:?}");
        cancel.cancel();
        old.await.unwrap();
        until("the old try ended", 30, || !st.engine.in_use(&tab)).await;
        assert_eq!(step(&run_all(&st, &job).await, "held_step").status, "success");
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

/// A scheduled job that can't start (no steps) uses up its slot: it isn't
/// tried again every tick, only at its next time; its lease is let go.
/// (A start that may work later gives the slot back: see
/// `a_failed_scheduled_start_gives_back_only_its_own_slot`.)
#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_cant_start_waits_for_its_next_time() {
    guarded(async {
        let st = state();
        let mut job = plain_job("Empty", vec![]);
        job.schedule = JobSchedule { enabled: true, minutes: 60, ..Default::default() };
        let job = st.workspace.save_job(job).unwrap();
        let before = databrain_workspace::now_ms() - 7_200_000;
        st.workspace.backdate_job_for_tests(&job.id, before).unwrap();
        assert!(databrain_app::jobs::tick(&st.job_ctx()).is_empty(), "a job without steps doesn't start");
        let after = st.workspace.get_job(&job.id).unwrap();
        assert!(after.last_scheduled_at.is_some_and(|t| t > before), "the slot is used: {:?}", after.last_scheduled_at);
        assert!(
            st.workspace.list_jobs().unwrap().iter().find(|j| j.id == job.id).unwrap().next_run_at.is_some_and(|t| t > databrain_workspace::now_ms()),
            "not due again until its next time"
        );
        assert!(st.workspace.job_lease(&job.id).unwrap().is_none(), "the lease is released");
        assert!(databrain_app::jobs::tick(&st.job_ctx()).is_empty());
    })
    .await;
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

/// A run the assistant started passes the AI rules on to the jobs that run
/// after it: their steps run as the AI's (sandbox, today's AI settings),
/// so a job of the user's on a connection where AI is off is refused. The
/// same job after a run the user started runs as before.
#[tokio::test(flavor = "multi_thread")]
async fn jobs_after_an_ai_run_follow_the_ai_rules() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let open = sqlite(&st, "open", &dir.path().join("open.db"), false);
        let closed = sqlite(&st, "no_ai", &dir.path().join("closed.db"), false);
        let mut p = st.workspace.get_connection(&closed).unwrap();
        p.ai_policy.ai_enabled = false;
        st.workspace.save_connection(p).unwrap();
        let up = st.workspace.save_job(plain_job("Upstream AI", vec![node("a", "up_ai", JobNodeKind::Query, Some(&open), "select 1 as x")])).unwrap();
        let mut down = plain_job("Mine after", vec![node("b", "mine_after", JobNodeKind::Query, Some(&closed), "select 2 as y")]);
        down.schedule.after_jobs = vec![up.id.clone()];
        let down = st.workspace.save_job(down).unwrap();

        // The user's run: the job after it runs as before.
        api::run_job(&st, &up.id, None).unwrap();
        assert_eq!(finished(&st, &up.id).await.status, "success");
        let r = next_run(&st, &down.id, None).await;
        assert_eq!((r.status.as_str(), r.ai), ("success", false), "{r:?}");

        // The assistant's run: the job after it runs under the AI rules and is refused.
        databrain_app::jobs::start(&st.job_ctx(), &up.id, "ai", None).unwrap();
        assert_eq!(finished(&st, &up.id).await.status, "success");
        let r2 = next_run(&st, &down.id, Some(r.id)).await;
        assert!(r2.ai && r2.trigger == "after:Upstream AI", "{r2:?}");
        let why = step(&r2, "mine_after").error.unwrap_or_default();
        assert!(r2.status == "error" && why.contains("AI is disabled"), "{why}");
        let log = st.workspace.job_run_log(&down.id, r2.id).unwrap();
        assert!(log[0].message.contains("every step runs under the AI rules"), "{:?}", log[0]);
    })
    .await
    .expect("test hung");
}

/// A job started after the assistant's run can't ask the user anything: a
/// step that needs the user's approval (rows copied out of a connection
/// that doesn't share its rows) is refused there, with why, while the
/// job's other steps run. The same job run by the user, or by the
/// assistant directly (approved with its steps), copies the rows.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_after_the_assistants_run_asks_nothing() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let open = sqlite(&st, "open", &dir.path().join("open.db"), false);
        let masked = sqlite(&st, "masked", &dir.path().join("masked.db"), false);
        let mut p = st.workspace.get_connection(&open).unwrap();
        p.ai_policy.allow_write = true;
        st.workspace.save_connection(p).unwrap();
        let mut p = st.workspace.get_connection(&masked).unwrap();
        p.ai_policy.share_result_rows = false;
        st.workspace.save_connection(p).unwrap();
        let up = st.workspace.save_job(plain_job("Ask up", vec![node("a", "ask_up", JobNodeKind::Query, Some(&open), "select 1 as x")])).unwrap();
        let mut copy = node("c", "ask_copy", JobNodeKind::Load, None, "");
        copy.target_connection_id = Some(open.clone());
        copy.target_table = Some("copied".into());
        let mut down = plain_job(
            "Ask copy",
            vec![
                node("b", "ask_rows", JobNodeKind::Query, Some(&masked), "select 7 as v"),
                copy,
                node("d", "ask_other", JobNodeKind::Query, Some(&open), "select 2 as y"),
            ],
        );
        down.edges = vec![JobEdge { from: "b".into(), to: "c".into() }];
        down.schedule.after_jobs = vec![up.id.clone()];
        let down = st.workspace.save_job(down).unwrap();

        // Refused: started after the assistant's run.
        databrain_app::jobs::start(&st.job_ctx(), &up.id, "ai", None).unwrap();
        assert_eq!(finished(&st, &up.id).await.status, "success");
        let r = next_run(&st, &down.id, None).await;
        assert!(r.ai, "{r:?}");
        let why = step(&r, "ask_copy").error.unwrap_or_default();
        assert!(why.contains("needs your approval, which can't be asked now") && why.contains("copies rows from"), "{why}");
        assert_eq!(step(&r, "ask_other").status, "success", "its other steps run");
        assert!(rows(&st, &open, "select count(*) from sqlite_master where name = 'copied'").await == vec![vec!["0".to_string()]]);

        // Allowed: the user's run of the same job.
        api::run_job(&st, &down.id, None).unwrap();
        let u = finished(&st, &down.id).await;
        assert!(!u.ai && u.status == "success", "{u:?}");
        assert_eq!(rows(&st, &open, "select v from copied").await, vec![vec!["7".to_string()]]);

        // Allowed: the assistant's own approved run of it.
        databrain_app::jobs::start(&st.job_ctx(), &down.id, "ai", None).unwrap();
        let a = finished(&st, &down.id).await;
        assert_eq!(a.status, "success", "{a:?}");
    })
    .await;
}

/// The AI rules pass along a chain of "run after" jobs, through the queue
/// and to the background runner (another process taking the queued
/// start): a job after one after the assistant's run is refused where AI is
/// off. A run the user starts afterwards, of the same jobs, is the user's.
#[tokio::test(flavor = "multi_thread")]
async fn the_ai_rules_follow_chains_the_queue_and_the_runner() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.db");
        let open_state = |kind: &str| {
            let mut st = AppState::new(Arc::new(Workspace::open(&path).unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
            st.jobs = Arc::new(databrain_app::jobs::JobsState::new(kind));
            st
        };
        let (app, cli) = (open_state("app"), open_state("cli"));
        let open = sqlite(&app, "open", &dir.path().join("open.db"), false);
        let closed = sqlite(&app, "no_ai", &dir.path().join("closed.db"), false);
        let mut p = app.workspace.get_connection(&closed).unwrap();
        p.ai_policy.ai_enabled = false;
        app.workspace.save_connection(p).unwrap();
        let up = app.workspace.save_job(plain_job("Chain up", vec![node("a", "chain_up", JobNodeKind::Query, Some(&open), "select 1 as x")])).unwrap();
        let mut mid = plain_job("Chain mid", vec![node("b", "chain_mid", JobNodeKind::Query, Some(&open), "select 2 as y")]);
        mid.schedule.after_jobs = vec![up.id.clone()];
        let mid = app.workspace.save_job(mid).unwrap();
        let mut leaf = plain_job("Chain leaf", vec![node("c", "chain_leaf", JobNodeKind::Query, Some(&closed), "select 3 as z")]);
        leaf.schedule.after_jobs = vec![mid.id.clone()];
        let leaf = app.workspace.save_job(leaf).unwrap();

        // The assistant's run: mid and leaf run under the AI rules; leaf is refused.
        databrain_app::jobs::start(&app.job_ctx(), &up.id, "ai", None).unwrap();
        assert_eq!(finished(&app, &up.id).await.status, "success");
        let m = next_run(&app, &mid.id, None).await;
        assert!(m.ai && m.status == "success", "{m:?}");
        let l = next_run(&app, &leaf.id, None).await;
        assert!(l.ai && l.status == "error", "{l:?}");
        assert!(step(&l, "chain_leaf").error.unwrap_or_default().contains("AI is disabled"));

        // Mid is busy when the assistant's run succeeds: queued under the AI
        // rules, and the background runner (another process) starts it so.
        app.jobs.hold_for_tests(&mid.id, true);
        databrain_app::jobs::start(&app.job_ctx(), &up.id, "ai", None).unwrap();
        assert_eq!(finished(&app, &up.id).await.status, "success");
        assert_eq!(app.workspace.queued_job_after().unwrap().len(), 1);
        app.jobs.hold_for_tests(&mid.id, false);
        assert!(databrain_app::jobs::tick_as(&cli.job_ctx(), "background").contains(&mid.id));
        let m2 = next_run(&cli, &mid.id, Some(m.id)).await;
        assert!(m2.ai && m2.status == "success" && m2.trigger == "after:Chain up", "{m2:?}");
        // …and its follower too, still under the AI rules.
        let l2 = next_run(&cli, &leaf.id, Some(l.id)).await;
        assert!(l2.ai && l2.status == "error", "{l2:?}");

        // The user's own run of the same jobs afterwards: theirs, all of it.
        api::run_job(&app, &up.id, None).unwrap();
        let u = finished(&app, &up.id).await;
        assert!(!u.ai && u.status == "success", "{u:?}");
        let m3 = next_run(&app, &mid.id, Some(m2.id)).await;
        assert!(!m3.ai && m3.status == "success", "{m3:?}");
        let l3 = next_run(&app, &leaf.id, Some(l2.id)).await;
        assert!(!l3.ai && l3.status == "success", "{l3:?}");
        // And a run of the leaf by hand: the user's.
        api::run_job(&app, &leaf.id, None).unwrap();
        let l4 = finished(&app, &leaf.id).await;
        assert!(!l4.ai && l4.status == "success", "{l4:?}");
    })
    .await;
}

/// Wait (bounded) for `job`'s newest run to be one other than `not` and finished.
async fn next_run(st: &AppState, job: &str, not: Option<i64>) -> JobRun {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(r) = st.workspace.job_runs(job, 1).unwrap().into_iter().next()
                && Some(r.id) != not
                && r.status != "running"
                && !st.jobs.is_running(job)
            {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the run finished")
}

/// "Run after": a job starts when the job it follows succeeds as a whole
/// (trigger `after:<name>`, named in its log), not when it fails, nor for a
/// run of chosen steps only.
#[tokio::test(flavor = "multi_thread")]
async fn jobs_run_after_the_job_they_follow_succeeds() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let conn = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let up = st.workspace.save_job(plain_job("Extract", vec![node("a", "extracted", JobNodeKind::Query, Some(&conn), "select 1 as x")])).unwrap();
        let mut down = plain_job("Report", vec![node("b", "reported", JobNodeKind::Query, Some(&conn), "select 2 as y")]);
        down.schedule.after_jobs = vec![up.id.clone()];
        let down = st.workspace.save_job(down).unwrap();

        api::run_job(&st, &up.id, None).unwrap();
        assert_eq!(finished(&st, &up.id).await.status, "success");
        let r = next_run(&st, &down.id, None).await;
        assert_eq!((r.trigger.as_str(), r.status.as_str()), ("after:Extract", "success"), "{r:?}");
        let log = st.workspace.job_run_log(&down.id, r.id).unwrap();
        assert!(log[0].message.contains("started after job \"Extract\" succeeded"), "{:?}", log[0]);

        // Only the chosen steps: not the job as a whole, the next one doesn't start.
        // (A follower is started, or queued, before the job it follows leaves
        // `running`: once `finished` returns, it would be there.)
        api::run_job(&st, &up.id, Some(vec!["a".into()])).unwrap();
        assert_eq!(finished(&st, &up.id).await.status, "success");
        assert_eq!(st.workspace.job_runs(&down.id, 10).unwrap().len(), 1);
        assert!(!st.jobs.is_running(&down.id) && st.workspace.queued_job_after().unwrap().is_empty());

        // A failed run doesn't start it.
        let mut broken = st.workspace.get_job(&up.id).unwrap();
        broken.nodes[0].sql = "select * from no_such_table".into();
        st.workspace.save_job(broken).unwrap();
        api::run_job(&st, &up.id, None).unwrap();
        assert_eq!(finished(&st, &up.id).await.status, "error");
        assert_eq!(st.workspace.job_runs(&down.id, 10).unwrap().len(), 1);
        assert!(!st.jobs.is_running(&down.id) && st.workspace.queued_job_after().unwrap().is_empty());

        // A loop is refused when saved.
        let mut looped = st.workspace.get_job(&up.id).unwrap();
        looped.schedule.after_jobs = vec![down.id.clone()];
        let e = api::save_job(&st, looped).unwrap_err();
        assert!(e.message.contains("Extract → Report → Extract"), "{}", e.message);
    })
    .await;
}

/// Missed runs: "each" makes up every missed time, one run after the other
/// (each counted from its own time); "skip" drops old ones without a run.
#[tokio::test(flavor = "multi_thread")]
async fn missed_runs_follow_the_jobs_policy() {
    guarded(async {
        use databrain_workspace::{MissedRuns, ScheduleMode};
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let conn = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let hour = 3_600_000;
        let now = databrain_workspace::now_ms();
        // Hourly in a zone whose hours start half an hour from now (an offset of
        // whole minutes), so the test never meets a new hour while it runs.
        let off = (hour / 2 - now).rem_euclid(hour) / 60_000;
        let zone = format!("+00:{off:02}");
        let slot = now - (now + off * 60_000).rem_euclid(hour); // the last hour start before now
        // Last counted 3.5 h before it: 4 missed times.
        let base = slot - 3 * hour - hour / 2;
        let mut job = plain_job("Hourly", vec![node("a", "hourly_rows", JobNodeKind::Query, Some(&conn), "select 1 as x")]);
        job.schedule = JobSchedule { enabled: true, minutes: 60, time_zone: zone, missed: MissedRuns::Each, ..Default::default() };
        let job = st.workspace.save_job(job).unwrap();
        st.workspace.backdate_job_for_tests(&job.id, base).unwrap();
        let mut marks = Vec::new();
        let mut last = None;
        for _ in 0..10 {
            if databrain_app::jobs::tick(&st.job_ctx()).is_empty() {
                break;
            }
            let r = next_run(&st, &job.id, last).await;
            assert_eq!((r.trigger.as_str(), r.status.as_str()), ("schedule", "success"));
            last = Some(r.id);
            marks.push(st.workspace.get_job(&job.id).unwrap().last_scheduled_at.unwrap());
        }
        let expected: Vec<i64> = (0..4).map(|i| base + hour / 2 + i * hour).collect();
        assert_eq!(marks, expected, "one run per missed hour, oldest first");

        // Skip: daily at a time 2 h ago, last counted days ago → no run, counted from now.
        let two_h_ago = chrono::Utc::now() - chrono::Duration::hours(2);
        let mut j = st.workspace.get_job(&job.id).unwrap();
        j.schedule = JobSchedule {
            enabled: true,
            mode: ScheduleMode::Daily,
            at: two_h_ago.format("%H:%M").to_string(),
            time_zone: "UTC".into(),
            missed: MissedRuns::Skip,
            ..Default::default()
        };
        st.workspace.save_job(j).unwrap();
        st.workspace.backdate_job_for_tests(&job.id, now - 3 * 24 * hour).unwrap();
        let runs = st.workspace.job_runs(&job.id, 50).unwrap().len();
        assert!(databrain_app::jobs::tick(&st.job_ctx()).is_empty());
        assert_eq!(st.workspace.job_runs(&job.id, 50).unwrap().len(), runs);
        assert!(st.workspace.get_job(&job.id).unwrap().last_scheduled_at.unwrap() >= now);
        assert!(databrain_app::jobs::tick(&st.job_ctx()).is_empty(), "and it isn't due again until tomorrow");

        // Skip, but the time was missed because the job was running then (a
        // run by hand that lasted past it): it runs once that run has ended.
        st.workspace.backdate_job_for_tests(&job.id, now - 3 * 24 * hour).unwrap();
        let mut busy = st.workspace.start_job_run(&job.id, "manual").unwrap();
        let slot = two_h_ago.timestamp_millis() / 60_000 * 60_000;
        st.workspace.backdate_job_run_for_tests(busy.id, slot - 5 * 60_000).unwrap();
        busy.status = "success".into();
        busy.finished_at = Some(slot + 20 * 60_000);
        st.workspace.update_job_run(&busy).unwrap();
        assert_eq!(databrain_app::jobs::tick(&st.job_ctx()), vec![job.id.clone()], "missed while busy: it runs");
        finished(&st, &job.id).await;
    })
    .await;
}

/// A scheduled run that fails on a connection with browser sign-in says so
/// in its log; a run by hand doesn't.
#[tokio::test(flavor = "multi_thread")]
async fn failed_scheduled_run_names_browser_sign_in() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let conn = sqlite(&st, "warehouse", &dir.path().join("w.db"), false);
        let mut p = st.workspace.get_connection(&conn).unwrap();
        p.config.auth = AuthMethod::OauthBrowser(Default::default());
        st.workspace.save_connection(p).unwrap();
        let mut job = plain_job("Nightly", vec![node("a", "nightly_fail", JobNodeKind::Query, Some(&conn), "select * from no_such_table")]);
        job.schedule = JobSchedule { enabled: true, minutes: 5, ..Default::default() };
        let job = st.workspace.save_job(job).unwrap();

        api::run_job(&st, &job.id, None).unwrap();
        let r = finished(&st, &job.id).await;
        assert_eq!(r.status, "error");
        let hint = |run: &JobRun| st.workspace.job_run_log(&job.id, run.id).unwrap().iter().any(|l| l.message.contains("signs in through the browser"));
        assert!(!hint(&r), "by hand: someone is there to sign in");

        st.workspace.backdate_job_for_tests(&job.id, databrain_workspace::now_ms() - 3_600_000).unwrap();
        assert_eq!(databrain_app::jobs::tick(&st.job_ctx()).len(), 1);
        let r = next_run(&st, &job.id, Some(r.id)).await;
        assert_eq!((r.trigger.as_str(), r.status.as_str()), ("schedule", "error"));
        assert!(hint(&r), "{:?}", st.workspace.job_run_log(&job.id, r.id).unwrap());
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

fn log_text(st: &AppState, job: &Job, run: &JobRun) -> Vec<String> {
    api::job_run_log(st, &job.id, run.id).unwrap().iter().map(|e| e.message.clone()).collect()
}

/// A connection to the live server `env` (`DATABRAIN_PG`, …) of `kind`.
fn live_conn(st: &AppState, env: &str, kind: ConnectorKind, user: &str, port: &str, database: &str) -> String {
    let host = std::env::var(format!("{env}_HOST")).unwrap_or_else(|_| panic!("{env}_HOST"));
    let mut c = ConnectionConfig::new(kind, AuthMethod::Password { user: std::env::var(format!("{env}_USER")).unwrap_or(user.into()) });
    c.host = Some(host);
    c.port = std::env::var(format!("{env}_PORT")).unwrap_or(port.into()).parse().ok();
    c.database = Some(database.into());
    api::save_connection(
        st,
        SaveConnectionArgs {
            profile: ConnectionProfile {
                id: String::new(),
                name: "live".into(),
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
    .id
}

/// Postgres: rows loaded by COPY end up exactly as the INSERT path writes
/// them (NULLs, quotes, tabs, newlines, backslashes, unicode, decimals,
/// NaN/inf, dates, timestamps with and without zone, binary, booleans),
/// into a table the load creates and into one with other column types
/// (text for booleans, numeric, jsonb, varchar); a column whose INSERT
/// would be cast differently (a fraction into an integer), binary into
/// text, and a table with row-level security go by INSERTs; a negative zero
/// is stored as the INSERT stores it.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test(flavor = "multi_thread")]
async fn live_postgres_copy_writes_what_inserts_write() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let st = state();
        let pg = live_conn(&st, "DATABRAIN_PG", ConnectorKind::Postgres, "postgres", "5432", "postgres");
        // Tables of this run only: two live runs at once (CI, a developer) don't clash.
        let sfx = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let t = |n: &str| format!("{n}_{sfx}");
        let src = "select * from (values \
            (1, 'it''s \"x\" ' || chr(9) || 'tab' || chr(10) || 'line' || chr(13) || 'cr \\ back', true, 2.5::double, 1.50::decimal(12, 3), date '2026-01-02', \
             timestamp '2026-01-02 03:04:05.123456', timestamptz '2026-01-02 03:04:05+02', '\\x00\\xFF\\x5C'::blob, 'ünïcødé ✓ 日本', '{\"a\": [1, \"b\"]}'), \
            (2, null, false, 'NaN'::double, -0.001::decimal(12, 3), null, null, null, ''::blob, '', 'null'), \
            (3, '\\N', null, '-inf'::double, null, date '1999-12-31', timestamp '1970-01-01 00:00:00', timestamptz '2026-06-30 23:59:59.5+00', null, null, null), \
            (4, '', null, 1e-7::double, 12345678.125::decimal(12, 3), null, null, null, null, 'end', '[]') \
            ) t(id, name, ok, f, d, day, ts, tsz, bin, uni, j)";
        let load = |id: &str, name: &str, table: &str, insert_only: bool, mode: LoadMode, before: Option<&str>| {
            let mut n = node(id, name, JobNodeKind::Load, None, "");
            n.target_connection_id = Some(pg.clone());
            n.target_table = Some(table.into());
            n.load_mode = mode;
            n.load_insert_only = insert_only;
            n.load_before_sql = before.map(str::to_string);
            n
        };
        let pre = |t: &str| {
            format!(
                "drop table if exists {t}; create table {t} (id bigint, name text, ok text, f double precision, d numeric(14, 3), day date, ts timestamp, \
                 tsz timestamptz, bin bytea, uni varchar(50), j jsonb)"
            )
        };
        let (pre_copy, pre_ins) = (pre(&t("db_pre_copy")), pre(&t("db_pre_ins")));
        let job = st
            .workspace
            .save_job(Job {
                id: String::new(),
                name: "copy vs inserts".into(),
                nodes: vec![
                    node("a", "copy_src", JobNodeKind::Query, None, src),
                    load("b", "by_copy", &t("db_new_copy"), false, LoadMode::Replace, None),
                    load("c", "by_inserts", &t("db_new_ins"), true, LoadMode::Replace, None),
                    load("d", "pre_by_copy", &t("db_pre_copy"), false, LoadMode::Append, Some(&pre_copy)),
                    load("e", "pre_by_inserts", &t("db_pre_ins"), true, LoadMode::Append, Some(&pre_ins)),
                ],
                edges: ["b", "c", "d", "e"].iter().map(|t| JobEdge { from: "a".into(), to: (*t).into() }).collect(),
                schedule: Default::default(),
                last_scheduled_at: None,
                params: Default::default(),
                param_set: None,
                limits: Default::default(),
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        let log = log_text(&st, &job, &run);
        for (step_name, copy) in [("by_copy", true), ("by_inserts", false), ("pre_by_copy", true), ("pre_by_inserts", false)] {
            assert_eq!(step(&run, step_name).rows, Some(4), "{step_name}");
            let entries = api::job_run_log(&st, &job.id, run.id).unwrap();
            let used = entries.iter().any(|e| e.step.as_deref() == Some(step_name) && e.message.contains("bulk load (COPY"));
            assert_eq!(used, copy, "{step_name}: {log:#?}");
        }
        for (a, b) in [(&t("db_new_copy"), &t("db_new_ins")), (&t("db_pre_copy"), &t("db_pre_ins"))] {
            let diff = format!(
                "select (select count(*) from (select * from {a} except all select * from {b}) x) + (select count(*) from (select * from {b} except all select * from {a}) y)"
            );
            assert_eq!(rows(&st, &pg, &diff).await, vec![vec!["0"]], "{a} vs {b}");
            // Column types the same too (the created tables).
            let types = |t: &str| format!("select string_agg(column_name || ' ' || data_type, ', ' order by ordinal_position) from information_schema.columns where table_name = '{t}'");
            assert_eq!(rows(&st, &pg, &types(a)).await, rows(&st, &pg, &types(b)).await);
        }
        let got = rows(&st, &pg, &format!("select name, ok, uni, encode(bin, 'hex'), f::text from db_pre_copy_{sfx} order by id")).await;
        assert_eq!(got[0][0], "it's \"x\" \ttab\nline\rcr \\ back");
        assert_eq!(got[0][1], "true");
        assert_eq!(got[0][2], "ünïcødé ✓ 日本");
        assert_eq!(got[0][3], "00ff5c");
        assert_eq!((got[1][4].as_str(), got[2][4].as_str()), ("NaN", "-Infinity"));
        assert_eq!(got[2][0], "\\N", "the text \\N is not NULL");

        // A fraction into an integer column: the INSERT rounds it, COPY would refuse: INSERTs.
        let mut ints = load("b", "to_ints", &t("db_ints"), false, LoadMode::Append, Some(&format!("drop table if exists db_ints_{sfx}; create table db_ints_{sfx} (f integer)")));
        ints.sql = String::new();
        let job = st
            .workspace
            .save_job(Job {
                id: String::new(),
                name: "ints".into(),
                nodes: vec![node("a", "int_src", JobNodeKind::Query, None, "select 2.6::double as f"), ints],
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
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert!(log_text(&st, &job, &run).iter().any(|l| l.contains("INSERTs, not COPY (f (Float64) into integer)")), "{:#?}", log_text(&st, &job, &run));
        assert_eq!(rows(&st, &pg, &format!("select f from db_ints_{sfx}")).await, vec![vec!["3"]]);

        // Binary into text (the INSERT's cast follows bytea_output), a table
        // with row-level security (COPY refuses it): INSERTs. A negative zero
        // by COPY is stored as the INSERT stores it (0).
        let one = |name: &str, table: &str, src: &str, before: &str| {
            let mut l = load("b", name, table, false, LoadMode::Append, Some(before));
            l.sql = String::new();
            Job {
                id: String::new(),
                name: format!("job {name}"),
                nodes: vec![node("a", &format!("{name}_src"), JobNodeKind::Query, None, src), l],
                edges: vec![JobEdge { from: "a".into(), to: "b".into() }],
                schedule: Default::default(),
                last_scheduled_at: None,
                params: Default::default(),
                param_set: None,
                limits: Default::default(),
                created_at: 0,
                updated_at: 0,
            }
        };
        for (name, table, src, before, why) in [
            ("to_text", &t("db_bin_text"), "select '\\x01ff'::blob as b", &format!("drop table if exists db_bin_text_{sfx}; create table db_bin_text_{sfx} (b text)"), Some("into text")),
            (
                "to_rls",
                &t("db_rls"),
                "select 1.5::double as f",
                &format!("drop table if exists db_rls_{sfx}; create table db_rls_{sfx} (f double precision); alter table db_rls_{sfx} enable row level security; \
                 alter table db_rls_{sfx} force row level security; create policy db_rls_all_{sfx} on db_rls_{sfx} using (true) with check (true)"),
                Some("row-level security"),
            ),
            ("neg_zero", &t("db_negz"), "select -0.0::double as f", &format!("drop table if exists db_negz_{sfx}; create table db_negz_{sfx} (f double precision)"), None),
        ] {
            let job = st.workspace.save_job(one(name, table, src, before)).unwrap();
            let run = run_all(&st, &job).await;
            assert_eq!(run.status, "success", "{name}: {run:?}");
            let log = log_text(&st, &job, &run);
            match why {
                Some(w) => assert!(log.iter().any(|l| l.contains("INSERTs, not COPY") && l.contains(w)), "{name}: {log:#?}"),
                None => assert!(log.iter().any(|l| l.contains("bulk load (COPY")), "{name}: {log:#?}"),
            }
        }
        assert_eq!(rows(&st, &pg, &format!("select f::text from db_negz_{sfx}")).await, vec![vec!["0"]]);
        assert_eq!(rows(&st, &pg, &format!("select count(*) from db_rls_{sfx}")).await, vec![vec!["1"]]);
        rows(&st, &pg, &format!("drop table db_new_copy_{sfx}, db_new_ins_{sfx}, db_pre_copy_{sfx}, db_pre_ins_{sfx}, db_ints_{sfx}, db_bin_text_{sfx}, db_rls_{sfx}, db_negz_{sfx}; select 1")).await;
    })
    .await
    .expect("in time");
}

/// Postgres, columns COPY must leave to INSERTs: a GENERATED ALWAYS
/// identity column the rows hold (COPY would write it; the INSERT refuses
/// it, as before) and a stored generated column; a BY DEFAULT identity
/// takes the rows' values either way (COPY). The dry run says which one a
/// load would use. Stop ends a COPY waiting for the table's lock: nothing
/// is kept and the connection loads again afterwards.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test(flavor = "multi_thread")]
async fn live_postgres_copy_columns_dry_run_and_stop() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let st = state();
        let pg = live_conn(&st, "DATABRAIN_PG", ConnectorKind::Postgres, "postgres", "5432", "postgres");
        let sfx = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let job_of = |name: &str, table: &str, src: &str, before: Option<&str>| {
            let mut l = node("b", name, JobNodeKind::Load, None, "");
            l.target_connection_id = Some(pg.clone());
            l.target_table = Some(table.into());
            l.load_mode = LoadMode::Append;
            l.load_before_sql = before.map(str::to_string);
            Job {
                id: String::new(),
                name: format!("job {name}"),
                nodes: vec![node("a", &format!("{name}_src"), JobNodeKind::Query, None, src), l],
                edges: vec![JobEdge { from: "a".into(), to: "b".into() }],
                schedule: Default::default(),
                last_scheduled_at: None,
                params: Default::default(),
                param_set: None,
                limits: Default::default(),
                created_at: 0,
                updated_at: 0,
            }
        };
        let (always, by_default, gen_t, locked) = (format!("db_idalw_{sfx}"), format!("db_iddef_{sfx}"), format!("db_gen_{sfx}"), format!("db_lock_{sfx}"));
        let keyed = format!("db_key_{sfx}");
        rows(
            &st,
            &pg,
            &format!(
                "create table {always} (id bigint generated always as identity, v text); \
                 create table {by_default} (id bigint generated by default as identity, v text); \
                 create table {gen_t} (a int, b int generated always as (a * 2) stored); \
                 create table {locked} (id bigint, v text); \
                 create table {keyed} (id bigint primary key, v text); select 1"
            ),
        )
        .await;

        // Identity ALWAYS: INSERTs, which refuse the value, as before COPY existed.
        let job = st.workspace.save_job(job_of(&format!("idalw_{sfx}"), &always, "select 5::bigint as id, 'x' as v", None)).unwrap();
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "error", "{run:?}");
        let log = log_text(&st, &job, &run);
        assert!(log.iter().any(|l| l.contains("INSERTs, not COPY") && l.contains("GENERATED ALWAYS AS IDENTITY")), "{log:#?}");
        assert!(run.error.as_deref().unwrap_or_default().contains("non-DEFAULT value"), "{run:?}");
        assert_eq!(rows(&st, &pg, &format!("select count(*) from {always}")).await, vec![vec!["0"]]);

        // Identity BY DEFAULT: COPY, the rows' values kept (as an INSERT keeps them).
        let job = st.workspace.save_job(job_of(&format!("iddef_{sfx}"), &by_default, "select 5::bigint as id, 'x' as v", None)).unwrap();
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert!(log_text(&st, &job, &run).iter().any(|l| l.contains("bulk load (COPY")), "{:#?}", log_text(&st, &job, &run));
        assert_eq!(rows(&st, &pg, &format!("select id::text, v from {by_default}")).await, vec![vec!["5", "x"]]);
        // The dry run says COPY would be used.
        let r = api::dry_run_job_step(&st, job.clone(), "b").await.unwrap();
        assert!(r.checks.iter().any(|c| c.message.as_deref().is_some_and(|m| m.contains("bulk load (COPY"))), "{r:?}");

        // A stored generated column the rows hold: INSERTs (which refuse it); not held: COPY, computed.
        let job = st.workspace.save_job(job_of(&format!("gen1_{sfx}"), &gen_t, "select 1 as a, 9 as b", None)).unwrap();
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "error", "{run:?}");
        assert!(log_text(&st, &job, &run).iter().any(|l| l.contains("INSERTs, not COPY") && l.contains("generated column")));
        let job = st.workspace.save_job(job_of(&format!("gen2_{sfx}"), &gen_t, "select 3 as a", None)).unwrap();
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert!(log_text(&st, &job, &run).iter().any(|l| l.contains("bulk load (COPY")));
        assert_eq!(rows(&st, &pg, &format!("select a, b from {gen_t}")).await, vec![vec!["3", "6"]]);

        // Stop while the COPY waits for the table's lock (held by another session).
        let locker = format!("locker-{sfx}");
        let hold = |sql: String| {
            let st = &st;
            let pg = pg.clone();
            let locker = locker.clone();
            async move {
                let out = st
                    .engine
                    .run_and_wait(
                        &st.hub,
                        RunRequest {
                            connection_id: pg,
                            tab_id: locker,
                            sql,
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
                assert!(out.iter().all(|o| o.error.is_none()), "{out:?}");
            }
        };
        hold(format!("begin; lock table {locked} in share mode")).await;
        let job = st.workspace.save_job(job_of(&format!("lock_{sfx}"), &locked, "select 1::bigint as id, 'x' as v", None)).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        let waiting = format!("select count(*) from pg_stat_activity where wait_event_type = 'Lock' and query like 'COPY %{locked}%'");
        let end = std::time::Instant::now() + Duration::from_secs(30);
        while rows(&st, &pg, &waiting).await != vec![vec!["1"]] {
            assert!(std::time::Instant::now() < end, "the COPY never waited for the lock");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let stopped = std::time::Instant::now();
        assert!(api::cancel_job(&st, &job.id));
        let run = finished(&st, &job.id).await;
        assert!(stopped.elapsed() < Duration::from_secs(8), "stopped in {:?}", stopped.elapsed());
        assert_eq!(run.status, "cancelled", "{run:?}");
        assert_eq!(rows(&st, &pg, &waiting).await, vec![vec!["0"]], "the COPY ended on the server");
        hold("rollback".into()).await;
        assert_eq!(rows(&st, &pg, &format!("select count(*) from {locked}")).await, vec![vec!["0"]], "nothing kept");
        // The load's connection works again.
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(rows(&st, &pg, &format!("select count(*) from {locked}")).await, vec![vec!["1"]]);

        // Stop while the COPY's end waits on a row: another session's
        // uncommitted row with the same key. The COPY's data is sent; its
        // finish waits; Stop ends it on the server too.
        hold(format!("begin; insert into {keyed} values (1, 'other')")).await;
        let job = st.workspace.save_job(job_of(&format!("key_{sfx}"), &keyed, "select 1::bigint as id, 'x' as v", None)).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        let waiting = format!("select count(*) from pg_stat_activity where wait_event_type = 'Lock' and query like 'COPY %{keyed}%'");
        let end = std::time::Instant::now() + Duration::from_secs(30);
        while rows(&st, &pg, &waiting).await != vec![vec!["1"]] {
            assert!(std::time::Instant::now() < end, "the COPY never waited for the row");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let stopped = std::time::Instant::now();
        assert!(api::cancel_job(&st, &job.id));
        let run = finished(&st, &job.id).await;
        assert!(stopped.elapsed() < Duration::from_secs(8), "stopped in {:?}", stopped.elapsed());
        assert_eq!(run.status, "cancelled", "{run:?}");
        assert_eq!(rows(&st, &pg, &waiting).await, vec![vec!["0"]], "the COPY ended on the server");
        hold("rollback".into()).await;
        assert_eq!(rows(&st, &pg, &format!("select count(*) from {keyed}")).await, vec![vec!["0"]], "nothing kept");

        rows(&st, &pg, &format!("drop table {always}, {by_default}, {gen_t}, {locked}, {keyed}; select 1")).await;
    })
    .await
    .expect("in time");
}

/// Rows of 100+ characters: `n` of them are about `n` × 110 bytes in memory.
fn wide_rows(n: u32) -> String {
    format!(
        "with recursive n(i) as (select 1 union all select i + 1 from n where i < {n}) select i, substr(replace(hex(zeroblob(50)), '0', 'x'), 1, 100) || i as s from n"
    )
}

/// Job steps' results have a memory budget of their own (a quarter of the
/// budget): past it a step's result moves to disk, whole, instead of being
/// cut, and it never frees the results of the user's tabs. When a run ends,
/// its steps' earlier versions are freed; the latest stays.
#[tokio::test(flavor = "multi_thread")]
async fn job_outputs_have_their_own_memory() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        st.engine.results().set_spill_root(dir.path().join("spill"));
        // 16 MB: a tab's result may keep 8 MB, the job steps' 4 MB together.
        st.engine.outputs().set_budget(16 << 20);
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        // A tab ran twice (about 4.4 MB each): the first result is no longer
        // shown (the budget may free it), the second is.
        let tab = "user-tab";
        for _ in 0..2 {
            let req = RunRequest {
                connection_id: src.clone(),
                tab_id: tab.into(),
                sql: wide_rows(40_000),
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin: Default::default(),
                session_key: None,
                output_name: None,
                params: Default::default(),
                output_masks: None,
            };
            let out = st.engine.run_and_wait(&st.hub, req, None).await.unwrap();
            assert!(out.iter().all(|o| o.error.is_none()));
        }
        let earlier = st.engine.outputs().list().into_iter().find(|o| o.tab_id == tab && !o.active).expect("the tab's earlier result");
        assert_eq!(earlier.state, databrain_query_engine::OutputState::Live);

        // A step of about 22 MB: more than the whole budget.
        let job = st.workspace.save_job(plain_job("big", vec![node("a", "big_rows", JobNodeKind::Query, Some(&src), &wide_rows(200_000))])).unwrap();
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(step(&run, "big_rows").rows, Some(200_000));
        let first = st.engine.outputs().resolve("big_rows").expect("results.big_rows");
        assert!(!first.truncated && first.spilled && first.rows == 200_000, "{first:?}");
        // What job steps keep in memory (not what is on disk) fits their share.
        let in_memory: usize = st
            .engine
            .outputs()
            .list()
            .iter()
            .filter(|o| o.tab_id.starts_with("job:") && o.state == databrain_query_engine::OutputState::Live && !o.spilled)
            .map(|o| o.bytes)
            .sum();
        assert!(in_memory <= st.engine.outputs().job_budget(), "{in_memory} in memory: {first:?}");
        // The tab's results are still there, the one it no longer shows too.
        assert_eq!(st.engine.outputs().get(&earlier.handle).unwrap().state, databrain_query_engine::OutputState::Live);

        // Again: the first run's result (now results.big_rows__1) is freed.
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(st.engine.outputs().get(&first.handle).unwrap().state, databrain_query_engine::OutputState::Evicted);
        let latest = st.engine.outputs().resolve("big_rows").unwrap();
        assert!(latest.handle != first.handle && latest.state == databrain_query_engine::OutputState::Live && latest.rows == 200_000, "{latest:?}");
        assert_eq!(st.engine.outputs().get(&earlier.handle).unwrap().state, databrain_query_engine::OutputState::Live);
    })
    .await
    .expect("in time");
}

/// An incremental step: its SQL gets `:watermark`, the start value on the
/// first run and then the largest value of its column; the mark moves only
/// when the step and the load after it succeed; Reset starts over.
#[tokio::test(flavor = "multi_thread")]
async fn incremental_steps_read_only_new_rows() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
        rows(&st, &src, "create table events (id integer, v text); insert into events values (1, 'a'), (2, 'b'), (3, 'c'); select 1").await;
        let mut read = node("a", "new_events", JobNodeKind::Query, Some(&src), "select id, v from events where id > :watermark order by id");
        read.incremental_column = Some("ID".into());
        read.incremental_start = Some("0".into());
        let mut load = node("b", "copied", JobNodeKind::Load, None, "");
        load.target_connection_id = Some(dst.clone());
        load.target_table = Some("copied".into());
        let mut job = plain_job("incremental", vec![read, load]);
        job.edges = vec![JobEdge { from: "a".into(), to: "b".into() }];
        let mut job = st.workspace.save_job(job).unwrap();
        let mark = |st: &AppState, job: &Job| api::job_watermark(st, &job.id, "a", "id").unwrap().map(|w| w.value);
        let log_text = |st: &AppState, job: &Job, run: &JobRun| {
            api::job_run_log(st, &job.id, run.id).unwrap().into_iter().map(|l| l.message).collect::<Vec<_>>().join("\n")
        };

        // First run: from the start value.
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(step(&run, "new_events").rows, Some(3));
        assert_eq!(mark(&st, &job).as_deref(), Some("3"));
        let log = log_text(&st, &job, &run);
        assert!(log.contains(":watermark = 0 (the start value") && log.contains("Incremental mark moved: largest id is now 3"), "{log}");
        // A mark of another column doesn't count.
        assert_eq!(api::job_watermark(&st, &job.id, "a", "v").unwrap(), None);

        // Second run: only the new rows.
        rows(&st, &src, "insert into events values (4, 'd'), (5, 'e'); select 1").await;
        let run = run_all(&st, &job).await;
        assert_eq!(step(&run, "new_events").rows, Some(2));
        assert!(log_text(&st, &job, &run).contains(":watermark = 3 (the step's mark from run #"));
        assert_eq!(mark(&st, &job).as_deref(), Some("5"));
        assert_eq!(rows(&st, &dst, "select count(*), max(id) from copied").await, vec![vec!["5", "5"]]);

        // The load fails (rolled back): the mark stays, the next run reads the row again.
        rows(&st, &src, "insert into events values (6, 'f'); select 1").await;
        job.nodes[1].load_after_sql = Some("insert into no_such_table values (1)".into());
        let mut job = st.workspace.save_job(job).unwrap();
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "error");
        assert_eq!(step(&run, "new_events").rows, Some(1));
        assert_eq!(mark(&st, &job).as_deref(), Some("5"));
        assert!(log_text(&st, &job, &run).contains("Incremental mark not moved: step copied after it didn't succeed"));
        job.nodes[1].load_after_sql = None;
        let mut job = st.workspace.save_job(job).unwrap();
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(step(&run, "new_events").rows, Some(1));
        assert_eq!(mark(&st, &job).as_deref(), Some("6"));
        assert_eq!(rows(&st, &dst, "select count(*) from copied").await, vec![vec!["6"]]);

        // Only the step, without its load: the load didn't run, so the mark stays.
        rows(&st, &src, "insert into events values (7, 'g'); select 1").await;
        api::run_job(&st, &job.id, Some(vec!["a".into()])).unwrap();
        let run = finished(&st, &job.id).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert_eq!(mark(&st, &job).as_deref(), Some("6"));

        // Then the whole job moves it; with no new rows it is kept.
        let run = run_all(&st, &job).await;
        assert_eq!(mark(&st, &job).as_deref(), Some("7"));
        let run2 = run_all(&st, &job).await;
        assert_eq!((step(&run, "new_events").rows, step(&run2, "new_events").rows), (Some(1), Some(0)));
        assert_eq!(mark(&st, &job).as_deref(), Some("7"));
        assert!(log_text(&st, &job, &run2).contains("Incremental mark kept: no rows"));

        // Reset: from the start value again.
        api::reset_job_watermark(&st, &job.id, "a").unwrap();
        assert_eq!(mark(&st, &job), None);
        let run = run_all(&st, &job).await;
        assert_eq!(step(&run, "new_events").rows, Some(7));
        assert_eq!(mark(&st, &job).as_deref(), Some("7"));

        // Without a start value (and no mark), or with a column the rows
        // don't have: the step fails and says why.
        api::reset_job_watermark(&st, &job.id, "a").unwrap();
        job.nodes[0].incremental_start = None;
        let mut job = st.workspace.save_job(job).unwrap();
        let err = step(&run_all(&st, &job).await, "new_events").error.unwrap_or_default();
        assert!(err.contains("Enter the start value of the incremental column ID"), "{err}");
        job.nodes[0].incremental_start = Some("0".into());
        job.nodes[0].incremental_column = Some("nope".into());
        let job = st.workspace.save_job(job).unwrap();
        let err = step(&run_all(&st, &job).await, "new_events").error.unwrap_or_default();
        assert!(err.contains("have no column nope") && err.contains("id, v"), "{err}");
    })
    .await
    .expect("in time");
}

/// Resume after an incremental step's load failed: the load succeeds
/// without the step running again, so the step's pending mark becomes its
/// mark and the next run doesn't load the same rows twice. A failed rerun
/// of the step drops the pending mark.
#[tokio::test(flavor = "multi_thread")]
async fn resume_moves_an_incremental_steps_pending_mark() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
        rows(&st, &src, "create table events (id integer, v text); insert into events values (1, 'a'), (2, 'b'), (3, 'c'); select 1").await;
        rows(&st, &dst, "create table copied (id integer, v text); select 1").await;
        let mut read = node("a", "events_in", JobNodeKind::Query, Some(&src), "select id, v from events where id > :watermark order by id");
        read.incremental_column = Some("id".into());
        read.incremental_start = Some("0".into());
        let mut load = node("b", "events_out", JobNodeKind::Load, None, "");
        load.target_connection_id = Some(dst.clone());
        load.target_table = Some("copied".into());
        load.load_after_sql = Some("insert into no_such_table values (1)".into());
        let mut job = plain_job("pending marks", vec![read, load]);
        job.edges = vec![JobEdge { from: "a".into(), to: "b".into() }];
        let job = st.workspace.save_job(job).unwrap();
        let mark = |st: &AppState| api::job_watermark(st, &job_id(st), "a", "id").unwrap().map(|w| w.value);
        fn job_id(st: &AppState) -> String {
            st.workspace.list_jobs().unwrap().into_iter().find(|j| j.name == "pending marks").unwrap().id
        }

        // The load fails: no mark yet, a pending one.
        let run = run_all(&st, &job).await;
        assert_eq!(run.status, "error", "{run:?}");
        assert_eq!(mark(&st), None);
        assert_eq!(st.workspace.job_watermark_pending(&job.id, "a", "id").unwrap().map(|p| p.value).as_deref(), Some("3"));

        // Fixed (as saved now: with its steps' last results), then resumed:
        // only the load runs, the mark moves to 3.
        let mut job = st.workspace.get_job(&job.id).unwrap();
        job.nodes[1].load_after_sql = None;
        let job = st.workspace.save_job(job).unwrap();
        api::resume_job(&st, &job.id).unwrap();
        let run = finished(&st, &job.id).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert!(run.nodes.iter().all(|n| n.node_id != "a"), "the step didn't run again: {run:?}");
        assert_eq!(mark(&st).as_deref(), Some("3"));
        assert_eq!(st.workspace.job_watermark_pending(&job.id, "a", "id").unwrap(), None);
        let log = api::job_run_log(&st, &job.id, run.id).unwrap().into_iter().map(|l| l.message).collect::<Vec<_>>().join("\n");
        assert!(log.contains("Incremental mark moved: the steps after it succeeded"), "{log}");

        // The next run reads only the new row: no row is loaded twice.
        rows(&st, &src, "insert into events values (4, 'd'); select 1").await;
        let run = run_all(&st, &job).await;
        assert_eq!(step(&run, "events_in").rows, Some(1));
        assert_eq!(rows(&st, &dst, "select count(*), count(distinct id) from copied").await, vec![vec!["4", "4"]]);

        // A pending mark is dropped when the step runs again and fails.
        let pending = databrain_workspace::PendingMark {
            column: "id".into(),
            value: "99".into(),
            value_type: Some("integer".into()),
            run_id: Some(run.id),
            taken_at: databrain_workspace::now_ms(),
        };
        st.workspace.set_job_watermark_pending(&job.id, "a", Some(&pending)).unwrap();
        let mut broken = st.workspace.get_job(&job.id).unwrap();
        broken.nodes[0].sql = "select id from no_such_table where id > :watermark".into();
        let broken = st.workspace.save_job(broken).unwrap();
        api::run_job(&st, &broken.id, Some(vec!["a".into()])).unwrap();
        assert_eq!(finished(&st, &broken.id).await.status, "error");
        assert_eq!(st.workspace.job_watermark_pending(&job.id, "a", "id").unwrap(), None);
        assert_eq!(mark(&st).as_deref(), Some("4"));
    })
    .await
    .expect("in time");
}

/// "Run after" a job that is busy: the start is queued, not lost, and the
/// next scheduler pass starts it once the job is free.
#[tokio::test(flavor = "multi_thread")]
async fn run_after_a_busy_job_is_queued() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let conn = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let up = st.workspace.save_job(plain_job("Upstream", vec![node("a", "up_done", JobNodeKind::Query, Some(&conn), "select 1 as x")])).unwrap();
        let mut down = plain_job("Follower", vec![node("b", "follower_done", JobNodeKind::Query, Some(&conn), "select 2 as y")]);
        down.schedule.after_jobs = vec![up.id.clone()];
        let down = st.workspace.save_job(down).unwrap();

        // The follower is running here (held) while the upstream succeeds: queued.
        st.jobs.hold_for_tests(&down.id, true);
        api::run_job(&st, &up.id, None).unwrap();
        assert_eq!(finished(&st, &up.id).await.status, "success");
        assert_eq!(st.workspace.queued_job_after().unwrap(), vec![(down.id.clone(), "after:Upstream".to_string())]);
        assert!(st.workspace.job_runs(&down.id, 5).unwrap().is_empty());
        // Still busy: a pass leaves it queued.
        assert!(!databrain_app::jobs::tick(&st.job_ctx()).contains(&down.id));
        assert_eq!(st.workspace.queued_job_after().unwrap().len(), 1);

        // Free: the next pass starts it, with the trigger it was queued with.
        st.jobs.hold_for_tests(&down.id, false);
        assert!(databrain_app::jobs::tick(&st.job_ctx()).contains(&down.id));
        let run = finished(&st, &down.id).await;
        assert_eq!((run.trigger.as_str(), run.status.as_str()), ("after:Upstream", "success"), "{run:?}");
        assert!(st.workspace.queued_job_after().unwrap().is_empty());
        // A user's start and one under the AI rules are kept apart, each run as
        // whose it is (the user's isn't put under the AI rules), oldest first;
        // the same kind queued twice is one start. Each is taken once.
        st.workspace.queue_job_after(&down.id, "after:Upstream", false).unwrap();
        st.workspace.queue_job_after(&down.id, "after:Upstream", true).unwrap();
        st.workspace.queue_job_after(&down.id, "after:Upstream", false).unwrap();
        assert_eq!(st.workspace.queued_job_after().unwrap().len(), 2);
        assert_eq!(st.workspace.take_job_after(&down.id).unwrap(), Some(("after:Upstream".to_string(), false)));
        assert_eq!(st.workspace.take_job_after(&down.id).unwrap(), Some(("after:Upstream".to_string(), true)));
        assert_eq!(st.workspace.take_job_after(&down.id).unwrap(), None);
    })
    .await
    .expect("in time");
}

/// A scheduled start that failed gives back its slot and lease when it may
/// work later, and only its own: not while a run of the job started here
/// meanwhile holds the lease, not when another process runs the job
/// ("busy"), not when another runner has marked the slot since.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_scheduled_start_gives_back_only_its_own_slot() {
    use databrain_query_engine::EngineError;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.db");
    let open = |kind: &str| {
        let mut st = AppState::new(Arc::new(Workspace::open(&path).unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
        st.jobs = Arc::new(databrain_app::jobs::JobsState::new(kind));
        st
    };
    let (app, cli) = (open("app"), open("cli"));
    let ctx = app.job_ctx();
    let job = app.workspace.save_job(plain_job("Slot", vec![node("a", "slot_step", JobNodeKind::Query, None, "select 1")])).unwrap();
    let transient = EngineError::new("internal", "database is locked");
    let before = Some(1_000);
    let claim_and_mark = |mark: i64| {
        assert!(app.workspace.claim_job(&job.id, &app.jobs.owner, databrain_workspace::now_ms() + 90_000).unwrap());
        app.workspace.mark_job_scheduled(&job.id, mark).unwrap();
    };
    let lease = || app.workspace.job_lease(&job.id).unwrap().map(|(o, _)| o);
    let slot = || app.workspace.get_job(&job.id).unwrap().last_scheduled_at;

    // Allowed: our mark, our lease, a start that may work later → given back.
    claim_and_mark(5_000);
    databrain_app::jobs::give_back(&ctx, &job.id, 5_000, before, &transient);
    assert_eq!((slot(), lease()), (before, None));

    // Refused: a run of the job started here meanwhile holds the lease and uses the slot.
    claim_and_mark(6_000);
    app.jobs.hold_for_tests(&job.id, true);
    databrain_app::jobs::give_back(&ctx, &job.id, 6_000, before, &transient);
    assert_eq!((slot(), lease()), (Some(6_000), Some(app.jobs.owner.clone())));
    // The scheduler's let-go (no run due, missed runs skipped) leaves it too.
    databrain_app::jobs::let_go(&ctx, &job.id);
    assert_eq!(lease(), Some(app.jobs.owner.clone()), "the running run keeps its lease");
    app.jobs.hold_for_tests(&job.id, false);
    databrain_app::jobs::let_go(&ctx, &job.id);
    assert_eq!(lease(), None, "let go when nothing of the job runs here");

    // Refused: another process holds the job ("busy"): it used the slot.
    claim_and_mark(7_000);
    databrain_app::jobs::give_back(&ctx, &job.id, 7_000, before, &EngineError::new("busy", "already running by the background runner"));
    assert_eq!(slot(), Some(7_000));
    app.workspace.release_job(&job.id, &app.jobs.owner).unwrap();

    // Refused: our lease ran out and the background runner took the job and
    // marked its own slot: ours isn't given back over it, its lease stays.
    claim_and_mark(8_000);
    app.workspace.expire_job_lease_for_tests(&job.id).unwrap();
    assert!(cli.workspace.claim_job(&job.id, &cli.jobs.owner, databrain_workspace::now_ms() + 90_000).unwrap());
    cli.workspace.mark_job_scheduled(&job.id, 9_000).unwrap();
    databrain_app::jobs::give_back(&ctx, &job.id, 8_000, before, &transient);
    assert_eq!((slot(), lease()), (Some(9_000), Some(cli.jobs.owner.clone())));
    // Same mark but not our lease: not given back either.
    cli.workspace.mark_job_scheduled(&job.id, 8_000).unwrap();
    databrain_app::jobs::give_back(&ctx, &job.id, 8_000, before, &transient);
    assert_eq!((slot(), lease()), (Some(8_000), Some(cli.jobs.owner.clone())));
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

/// An incremental step that reads is tried again after a connection error
/// (its `:watermark` is a value, so its SQL is still a read); one that
/// writes isn't.
#[tokio::test(flavor = "multi_thread")]
async fn incremental_reads_are_tried_again() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let st = state();
        st.jobs.set_retry_delay(Duration::from_secs(3));
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("later");
        let later = sqlite(&st, "Later", &folder.join("later.db"), false);
        let mut read = node("n1", "inc_read_later", JobNodeKind::Query, Some(&later), "select 42 as id where 42 > :watermark");
        read.incremental_column = Some("id".into());
        read.incremental_start = Some("0".into());
        read.limits.retries = Some(2);
        let job = st.workspace.save_job(plain_job("Incremental retry", vec![read])).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        until("the first try failed", 30, || logged(&st, &job.id, "Try 1 of 3 failed")).await;
        std::fs::create_dir(&folder).unwrap();
        let run = finished(&st, &job.id).await;
        let log = api::job_run_log(&st, &job.id, run.id).unwrap();
        assert_eq!(run.status, "success", "{log:#?}");
        assert_eq!(step(&run, "inc_read_later").attempt, Some(2));

        // Writing SQL with :watermark: not tried again.
        std::fs::remove_dir_all(&folder).unwrap();
        let mut write = node("n1", "inc_write_later", JobNodeKind::Query, Some(&later), "delete from t where id < :watermark; select 1 as id");
        write.incremental_column = Some("id".into());
        write.incremental_start = Some("0".into());
        write.limits.retries = Some(2);
        let job2 = st.workspace.save_job(plain_job("Incremental no retry", vec![write])).unwrap();
        let run = run_all(&st, &job2).await;
        assert_eq!((run.status.as_str(), step(&run, "inc_write_later").attempt), ("error", Some(1)), "{run:?}");
        let log = api::job_run_log(&st, &job2.id, run.id).unwrap();
        assert!(log.iter().any(|l| l.message.starts_with("Not tried again: its SQL may write")), "{log:?}");
    })
    .await
    .expect("test hung");
}

/// `databrain-jobs tick` end to end: every run it started is reported (a
/// job made up twice: both), a failed one is notified and fails the
/// command, and runs of another process (the app) aren't its own.
#[tokio::test(flavor = "multi_thread")]
async fn the_background_tick_reports_its_own_runs() {
    use databrain_app::cli::jobs::{background_tick, tick_report};
    use databrain_workspace::MissedRuns;
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
        // The first run fails (the check wants 2 rows), the second succeeds.
        rows(&app, &conn, "create table c (x integer); create table u (n integer check (n > 1)); select 1").await;
        let sql = "insert into c values (1); insert into u select count(*) from c; select count(*) as n from c";
        let mut job = plain_job("Twice", vec![node("a", "twice_rows", JobNodeKind::Query, Some(&conn), sql)]);
        // Hourly, two missed hours, one run each; in a zone whose hours start
        // half an hour from now, so no new hour comes while the test runs.
        let hour = 3_600_000;
        let now = databrain_workspace::now_ms();
        let off = (hour / 2 - now).rem_euclid(hour) / 60_000;
        job.schedule = JobSchedule { enabled: true, minutes: 60, time_zone: format!("+00:{off:02}"), missed: MissedRuns::Each, ..Default::default() };
        let job = app.workspace.save_job(job).unwrap();
        let slot = now - (now + off * 60_000).rem_euclid(hour);
        app.workspace.backdate_job_for_tests(&job.id, slot - hour - hour / 2).unwrap();
        // A job the app runs meanwhile: not the runner's.
        let other = app.workspace.save_job(plain_job("App's", vec![node("a", "apps_rows", JobNodeKind::Query, Some(&conn), "select 1 as x")])).unwrap();
        api::run_job(&app, &other.id, None).unwrap();

        let runs = background_tick(&cli, Duration::from_secs(120)).await;
        assert_eq!(
            runs.iter().map(|r| (r.job_id.as_str(), r.trigger.as_str(), r.status.as_str())).collect::<Vec<_>>(),
            vec![(job.id.as_str(), "background", "error"), (job.id.as_str(), "background", "success")]
        );
        let report = tick_report(&cli.workspace, &runs);
        assert!(report.failed, "{report:?}");
        assert_eq!(report.lines, vec!["Twice: error".to_string(), "Twice: success".to_string()]);
        assert_eq!(report.notices.len(), 1, "{report:?}");
        assert_eq!(report.notices[0].0, "DataBrain: scheduled job failed");
        assert!(report.notices[0].1.starts_with("Twice: twice_rows:"), "{report:?}");
        assert!(cli.workspace.get_setting(databrain_app::background::LAST_TICK_SETTING).unwrap().is_some());
        finished(&app, &other.id).await;
        // Nothing due: nothing reported, success.
        let runs = background_tick(&cli, Duration::from_secs(60)).await;
        assert!(runs.is_empty(), "{runs:?}");
        assert_eq!(tick_report(&cli.workspace, &runs), Default::default());
    })
    .await;
}

/// The background runner's deadline: runs still going are stopped, reported
/// and notified as stopped (not failed), and the runner ends.
#[tokio::test(flavor = "multi_thread")]
async fn the_background_tick_stops_at_its_deadline() {
    use databrain_app::cli::jobs::{background_tick, tick_report};
    guarded(async {
        let st = state();
        let dir = tempfile::tempdir().unwrap();
        let a = sqlite(&st, "A", &dir.path().join("a.db"), false);
        let slow = "with recursive c(i) as (select 1 union all select i + 1 from c where i < 300000000) select count(*) from c";
        let mut job = plain_job("Endless", vec![node("a", "endless_rows", JobNodeKind::Query, Some(&a), slow)]);
        job.schedule = JobSchedule { enabled: true, minutes: 60, ..Default::default() };
        let job = st.workspace.save_job(job).unwrap();
        st.workspace.backdate_job_for_tests(&job.id, databrain_workspace::now_ms() - 7_200_000).unwrap();
        let t = std::time::Instant::now();
        let runs = background_tick(&st, Duration::from_secs(2)).await;
        assert!(t.elapsed() < Duration::from_secs(60), "{:?}", t.elapsed());
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "cancelled", "{:?}", runs[0]);
        assert!(runs[0].error.as_deref().unwrap_or_default().contains("longest time"), "{:?}", runs[0]);
        let report = tick_report(&st.workspace, &runs);
        assert!(!report.failed);
        assert_eq!(report.notices[0].0, "DataBrain: scheduled job stopped");
        assert!(!st.jobs.is_running(&job.id));
    })
    .await;
}

/// A follower queued while another process holds it stays queued there;
/// once free, the app and the runner look at the same moment and it starts
/// once.
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_follower_held_elsewhere_starts_once() {
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
        let up = app.workspace.save_job(plain_job("Leader", vec![node("a", "leader_rows", JobNodeKind::Query, Some(&conn), "select 1 as x")])).unwrap();
        let mut down = plain_job("Held follower", vec![node("b", "held_rows", JobNodeKind::Query, Some(&conn), "select 2 as y")]);
        down.schedule.after_jobs = vec![up.id.clone()];
        let down = app.workspace.save_job(down).unwrap();

        // The runner holds the follower: the leader's success queues it.
        assert!(cli.workspace.claim_job(&down.id, &cli.jobs.owner, databrain_workspace::now_ms() + 90_000).unwrap());
        api::run_job(&app, &up.id, None).unwrap();
        assert_eq!(finished(&app, &up.id).await.status, "success");
        assert_eq!(app.workspace.queued_job_after().unwrap().len(), 1);
        // The app's pass leaves it while the other process holds it.
        assert!(!databrain_app::jobs::tick(&app.job_ctx()).contains(&down.id));
        assert_eq!(app.workspace.queued_job_after().unwrap().len(), 1, "still queued");
        assert!(app.workspace.job_runs(&down.id, 5).unwrap().is_empty());

        // Free: both look at once, one starts it.
        cli.workspace.release_job(&down.id, &cli.jobs.owner).unwrap();
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
        let started = a.iter().chain(&b).filter(|id| **id == down.id).count();
        assert_eq!(started, 1, "app {a:?}, runner {b:?}");
        let ran_in = if a.contains(&down.id) { &app } else { &cli };
        assert_eq!(finished(ran_in, &down.id).await.status, "success");
        assert_eq!(app.workspace.job_runs(&down.id, 5).unwrap().len(), 1);
        assert!(app.workspace.queued_job_after().unwrap().is_empty());
    })
    .await;
}

/// A runner that died (no word from it for two minutes): its run is
/// closed and its lease let go, so the job runs again on schedule. A start
/// that can't be recorded isn't run (it would run again and again).
#[tokio::test(flavor = "multi_thread")]
async fn dead_runners_and_unrecorded_starts() {
    guarded(async {
        let st = state();
        let dir = tempfile::tempdir().unwrap();
        let conn = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let mut job = plain_job("Hourly", vec![node("a", "hourly_rows", JobNodeKind::Query, Some(&conn), "select 1 as x")]);
        job.schedule = JobSchedule { enabled: true, minutes: 60, ..Default::default() };
        let job = st.workspace.save_job(job).unwrap();
        let now = databrain_workspace::now_ms();
        st.workspace.backdate_job_for_tests(&job.id, now - 7_200_000).unwrap();

        // A dead runner's run and lease (its lease ran out; it last reported 5 minutes ago).
        assert!(st.workspace.claim_job(&job.id, "cli:dead", now - 1).unwrap());
        let dead = st.workspace.start_job_run(&job.id, "background").unwrap();
        st.workspace.backdate_job_run_for_tests(dead.id, now - 300_000).unwrap();
        // A live runner's run is left alone (allowed case of the check).
        let live_job = st.workspace.save_job(plain_job("Live", vec![node("a", "live_rows", JobNodeKind::Query, Some(&conn), "select 1 as x")])).unwrap();
        let live = st.workspace.start_job_run(&live_job.id, "background").unwrap();
        st.workspace.touch_job_run(live.id, "cli:alive").unwrap();

        // Recording the start fails: not started (and not again on the next pass).
        st.workspace.fail_job_marks_for_tests(true).unwrap();
        for _ in 0..3 {
            assert!(databrain_app::jobs::tick(&st.job_ctx()).is_empty());
        }
        let closed = st.workspace.job_run(&job.id, dead.id).unwrap().unwrap();
        assert_eq!(closed.status, "cancelled", "the dead runner's run is closed: {closed:?}");
        assert_eq!(st.workspace.job_run(&live_job.id, live.id).unwrap().unwrap().status, "running", "a live runner's run is not");
        assert_eq!(st.workspace.job_runs(&job.id, 10).unwrap().len(), 1, "no run started while its start can't be recorded");
        assert_eq!(st.workspace.job_lease(&job.id).unwrap(), None, "the lease is let go");

        // Recorded again: it runs, once.
        st.workspace.fail_job_marks_for_tests(false).unwrap();
        assert_eq!(databrain_app::jobs::tick(&st.job_ctx()), vec![job.id.clone()]);
        assert_eq!(finished(&st, &job.id).await.status, "success");
        assert!(databrain_app::jobs::tick(&st.job_ctx()).is_empty(), "not due again");
        assert_eq!(st.workspace.job_runs(&job.id, 10).unwrap().len(), 2);
    })
    .await;
}

/// An incremental step whose rows two loads read: its new mark waits until
/// both have loaded those rows. "Run this step" on the step, then on one
/// load, doesn't move it (the other load's last success had older rows:
/// moving it would skip the new row there for good); the other load
/// running too does.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_mark_waits_for_every_step_after_it() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
        rows(&st, &src, "create table events (id integer); insert into events values (1), (2), (3); select 1").await;
        rows(&st, &dst, "create table one (id integer); create table two (id integer); select 1").await;
        let mut read = node("a", "fanned", JobNodeKind::Query, Some(&src), "select id from events where id > :watermark order by id");
        read.incremental_column = Some("id".into());
        read.incremental_start = Some("0".into());
        let load = |id: &str, name: &str, table: &str| {
            let mut l = node(id, name, JobNodeKind::Load, None, "");
            l.target_connection_id = Some(dst.clone());
            l.target_table = Some(table.into());
            l
        };
        let mut job = plain_job("fan out", vec![read, load("b", "to_one", "one"), load("c", "to_two", "two")]);
        job.edges = vec![JobEdge { from: "a".into(), to: "b".into() }, JobEdge { from: "a".into(), to: "c".into() }];
        let job = st.workspace.save_job(job).unwrap();
        let mark = |st: &AppState| api::job_watermark(st, &job.id, "a", "id").unwrap().map(|w| w.value);
        let only = |ids: &[&str]| Some(ids.iter().map(|s| s.to_string()).collect::<Vec<_>>());

        assert_eq!(run_all(&st, &job).await.status, "success");
        assert_eq!(mark(&st).as_deref(), Some("3"));

        rows(&st, &src, "insert into events values (4); select 1").await;
        api::run_job(&st, &job.id, only(&["a"])).unwrap();
        assert_eq!(finished(&st, &job.id).await.status, "success");
        assert_eq!(mark(&st).as_deref(), Some("3"), "the loads haven't run: pending");
        api::run_job(&st, &job.id, only(&["b"])).unwrap();
        assert_eq!(finished(&st, &job.id).await.status, "success");
        assert_eq!(mark(&st).as_deref(), Some("3"), "to_two's last success had only the older rows");
        assert_eq!(rows(&st, &dst, "select count(*) from one union all select count(*) from two").await, vec![vec!["4"], vec!["3"]]);
        api::run_job(&st, &job.id, only(&["c"])).unwrap();
        assert_eq!(finished(&st, &job.id).await.status, "success");
        assert_eq!(mark(&st).as_deref(), Some("4"), "both loads have the new row");

        // The next run reads nothing new: no row lost or loaded twice.
        let run = run_all(&st, &job).await;
        assert_eq!(step(&run, "fanned").rows, Some(0));
        let counts = "select count(*), count(distinct id) from one union all select count(*), count(distinct id) from two";
        assert_eq!(rows(&st, &dst, counts).await, vec![vec!["4", "4"], vec!["4", "4"]]);
    })
    .await;
}

/// Resume when the failed step is two steps below the incremental one: the
/// step between them still holds this run's rows (it isn't run again), so
/// the mark moves once the load succeeds, and the next run loads nothing twice.
#[tokio::test(flavor = "multi_thread")]
async fn resume_moves_a_mark_two_steps_up() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let dst = sqlite(&st, "dst", &dir.path().join("dst.db"), false);
        rows(&st, &src, "create table events (id integer); insert into events values (1), (2), (3); select 1").await;
        rows(&st, &dst, "create table copied (id integer); select 1").await;
        let mut read = node("a", "chain_in", JobNodeKind::Query, Some(&src), "select id from events where id > :watermark order by id");
        read.incremental_column = Some("id".into());
        read.incremental_start = Some("0".into());
        let middle = node("t", "chain_mid", JobNodeKind::Query, None, "select * from results.chain_in");
        let mut load = node("l", "chain_out", JobNodeKind::Load, None, "");
        load.target_connection_id = Some(dst.clone());
        load.target_table = Some("copied".into());
        load.load_after_sql = Some("insert into no_such_table values (1)".into());
        let mut job = plain_job("chain", vec![read, middle, load]);
        job.edges = vec![JobEdge { from: "a".into(), to: "t".into() }, JobEdge { from: "t".into(), to: "l".into() }];
        let mut job = st.workspace.save_job(job).unwrap();
        let mark = |st: &AppState, id: &str| api::job_watermark(st, id, "a", "id").unwrap().map(|w| w.value);

        assert_eq!(run_all(&st, &job).await.status, "error");
        assert_eq!(mark(&st, &job.id), None);
        // Fixed as the user does it: the saved job, with its steps' last runs.
        job = st.workspace.get_job(&job.id).unwrap();
        job.nodes[2].load_after_sql = None;
        let job = st.workspace.save_job(job).unwrap();
        api::resume_job(&st, &job.id).unwrap();
        let run = finished(&st, &job.id).await;
        assert_eq!(run.status, "success", "{run:?}");
        assert!(run.nodes.iter().all(|n| n.node_id == "l"), "only the load ran again: {run:?}");
        assert_eq!(mark(&st, &job.id).as_deref(), Some("3"));

        rows(&st, &src, "insert into events values (4); select 1").await;
        let run = run_all(&st, &job).await;
        assert_eq!(step(&run, "chain_in").rows, Some(1));
        assert_eq!(rows(&st, &dst, "select count(*), count(distinct id) from copied").await, vec![vec!["4", "4"]]);
    })
    .await;
}

/// Reset while the job runs is refused (the run's end would set the mark
/// it read again); once it has ended, Reset works.
#[tokio::test(flavor = "multi_thread")]
async fn a_mark_is_not_reset_while_its_job_runs() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let src = sqlite(&st, "src", &dir.path().join("src.db"), false);
        let mut read = node("a", "quick_mark", JobNodeKind::Query, Some(&src), "select 5 as id where 5 > :watermark");
        read.incremental_column = Some("id".into());
        read.incremental_start = Some("0".into());
        let slow = node("b", "slow_beside", JobNodeKind::Query, Some(&src), SLOW);
        let job = st.workspace.save_job(plain_job("reset while running", vec![read, slow])).unwrap();
        api::run_job(&st, &job.id, None).unwrap();
        until("the slow step running", 20, || step_status(&st, &job.id, "b").as_deref() == Some("running")).await;
        let e = api::reset_job_watermark(&st, &job.id, "a").unwrap_err();
        assert!(e.message.contains("running"), "{e:?}");
        assert!(api::cancel_job(&st, &job.id));
        finished(&st, &job.id).await;
        api::reset_job_watermark(&st, &job.id, "a").unwrap();
        assert_eq!(api::job_watermark(&st, &job.id, "a", "id").unwrap(), None);
    })
    .await;
}

/// A mark goes back into the SQL as its column's type: a text column's
/// number-like value as a string, a timestamp with its fraction and offset,
/// binary as a hex literal. Each next run reads nothing again.
#[tokio::test(flavor = "multi_thread")]
async fn marks_keep_their_columns_type() {
    guarded(async {
        let dir = tempfile::tempdir().unwrap();
        let st = state();
        let db = duckdb(&st, "typed", &dir.path().join("typed.duckdb"));
        rows(
            &st,
            &db,
            "create table src (code varchar, ts timestamptz, bin blob); \
             insert into src values ('9', timestamptz '2026-01-02 03:04:05.123456+00', '\\x01\\x02'::blob), \
             ('20260102', timestamptz '2026-01-02 03:04:05.123457+00', '\\x01\\xff'::blob); select 1",
        )
        .await;
        for (name, col, want) in
            [("by_code", "code", "> '9'"), ("by_ts", "ts", "> TIMESTAMPTZ '2026-01-02 03:04:05.123457"), ("by_bin", "bin", "> unhex('01ff')")]
        {
            // The first run starts below every value (NULL: all rows).
            let mut n = node("a", name, JobNodeKind::Query, Some(&db), &format!("select {col} from src where :watermark is null or {col} > :watermark"));
            n.incremental_column = Some(col.into());
            n.incremental_start = Some("null".into());
            let job = st.workspace.save_job(plain_job(&format!("typed {name}"), vec![n])).unwrap();
            let first = run_all(&st, &job).await;
            assert_eq!(first.status, "success", "{name}: {first:?}");
            assert_eq!(step(&first, name).rows, Some(2), "{name}");
            let second = run_all(&st, &job).await;
            assert_eq!(second.status, "success", "{name}: {second:?}");
            let log = api::job_run_log(&st, &job.id, second.id).unwrap();
            let sql: Vec<String> = log.iter().filter_map(|l| l.sql.clone()).collect();
            assert!(sql.iter().any(|s| s.contains(want)), "{name}: {sql:#?}");
            assert_eq!(step(&second, name).rows, Some(0), "{name}: the largest row isn't read again");
        }
    })
    .await;
}

/// Live (Postgres, MySQL): marks of a text column holding numbers, of a
/// timestamp with fractional seconds (and a zone on Postgres) and of binary
/// go back as literals of their type: each next run reads nothing again
/// (and fails nowhere, as `text > 9` would on Postgres).
#[ignore = "live: needs DATABRAIN_PG_HOST and DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
#[tokio::test(flavor = "multi_thread")]
async fn live_marks_keep_their_columns_type() {
    guarded(async {
        let st = state();
        let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let servers = [
            (
                "DATABRAIN_PG",
                ConnectorKind::Postgres,
                "postgres",
                "5432",
                "postgres",
                "code text, ts timestamptz(6), bin bytea",
                "'\\x01ff'::bytea",
                "'\\x0102'::bytea",
            ),
            ("DATABRAIN_MYSQL", ConnectorKind::Mysql, "root", "3306", "mysql", "code varchar(20), ts datetime(6), bin varbinary(8)", "x'01ff'", "x'0102'"),
        ];
        for (env, kind, user, port, database, cols, hi, lo) in servers {
            let conn = live_conn(&st, env, kind, user, port, database);
            let table = format!("db_marks_{suffix}");
            let ts_hi = if kind == ConnectorKind::Postgres { "'2026-01-02 03:04:05.123457+02'" } else { "'2026-01-02 03:04:05.123457'" };
            let ts_lo = if kind == ConnectorKind::Postgres { "'2026-01-02 03:04:05.123456+02'" } else { "'2026-01-02 03:04:05.123456'" };
            rows(&st, &conn, &format!("create table {table} ({cols}); select 1")).await;
            rows(&st, &conn, &format!("insert into {table} values ('9', {ts_lo}, {lo}), ('20260102', {ts_hi}, {hi}); select 1")).await;
            for col in ["code", "ts", "bin"] {
                let name = format!("mark_{col}_{}", if kind == ConnectorKind::Postgres { "pg" } else { "my" });
                let mut n =
                    node("a", &name, JobNodeKind::Query, Some(&conn), &format!("select {col} from {table} where :watermark is null or {col} > :watermark"));
                n.incremental_column = Some(col.into());
                n.incremental_start = Some("null".into());
                let job = st.workspace.save_job(plain_job(&format!("{env} {col}"), vec![n])).unwrap();
                let first = run_all(&st, &job).await;
                assert_eq!(first.status, "success", "{env} {col}: {first:?}");
                assert_eq!(step(&first, &name).rows, Some(2), "{env} {col}");
                let second = run_all(&st, &job).await;
                assert_eq!(second.status, "success", "{env} {col}: {second:?}");
                assert_eq!(step(&second, &name).rows, Some(0), "{env} {col}: {:#?}", api::job_run_log(&st, &job.id, second.id).unwrap());
            }
            rows(&st, &conn, &format!("drop table {table}; select 1")).await;
        }
    })
    .await;
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
