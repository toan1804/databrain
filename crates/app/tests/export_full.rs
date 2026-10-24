//! Export of every row: the result's query runs again without the row limit.

use std::sync::Arc;

use databrain_app::api::{self, AppState};
use databrain_auth::MemoryStore;
use databrain_export::{ExportFormat, ExportOptions};
use databrain_query_engine::{EventSink, JobEvent, RunRequest};
use databrain_workspace::Workspace;

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
/// Records UI events.
#[derive(Default)]
struct Ui(parking_lot::Mutex<Vec<(String, serde_json::Value)>>);
impl databrain_app::ai_api::UiBridge for Ui {
    fn emit(&self, ch: &str, v: serde_json::Value) {
        self.0.lock().push((ch.to_string(), v));
    }
    fn open_url(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

async fn run(st: &AppState, conn: &str, sql: &str, limit: Option<usize>) -> databrain_query_engine::StatementOutcomeView {
    let out = st
        .engine
        .run_and_wait(&st.hub, RunRequest { connection_id: conn.into(), tab_id: "t".into(), sql: sql.into(), base_offset: 0, row_limit: limit, confirmed: true, origin: Default::default(), session_key: None, output_name: None, params: Default::default() }, None)
        .await
        .unwrap();
    out.into_iter().next_back().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn exports_all_rows_past_the_row_limit() {
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let duck = st.engine.results_connection().unwrap().id;
    let r = run(&st, &duck, "select i, 'row ' || i as label from range(5000) t(i) order by i", Some(100)).await;
    let info = r.result.unwrap();
    assert!(info.truncated);
    assert_eq!(info.total_rows, 100);
    let dir = tempfile::tempdir().unwrap();
    for (fmt, file) in [(ExportFormat::Csv, "all.csv"), (ExportFormat::Parquet, "all.parquet")] {
        let path = dir.path().join(file);
        let n = api::export_full_result(&st, &info.id, ExportOptions::new(fmt), path.to_string_lossy().into_owned(), "x1").await.unwrap();
        assert_eq!(n, 5000, "{fmt:?}");
        assert!(path.exists() && !dir.path().join(format!("{file}.partial")).exists());
    }
    let csv = std::fs::read_to_string(dir.path().join("all.csv")).unwrap();
    assert_eq!(csv.lines().count(), 5001);
    assert_eq!(csv.lines().last(), Some("4999,row 4999"));

    // Writes are never run again.
    let w = run(&st, &duck, "create table t as select 1 as a", None).await;
    assert!(w.error.is_none());
    let ins = run(&st, &duck, "insert into t values (2) returning a", None).await;
    if let Some(res) = ins.result {
        let e = api::export_full_result(&st, &res.id, ExportOptions::new(ExportFormat::Csv), dir.path().join("x.csv").to_string_lossy().into_owned(), "x2").await.unwrap_err();
        assert!(e.message.contains("Only queries that read data"), "{}", e.message);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_progress_and_remembers_the_file() {
    let ui = Arc::new(Ui::default());
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), ui.clone());
    let duck = st.engine.results_connection().unwrap().id;
    let r = run(&st, &duck, "select i from range(50000) t(i)", Some(10)).await;
    let info = r.result.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.csv");
    let n = api::export_full_result(&st, &info.id, ExportOptions::new(ExportFormat::Csv), path.to_string_lossy().into_owned(), "p1").await.unwrap();
    assert_eq!(n, 50000);
    let events: Vec<serde_json::Value> = ui.0.lock().iter().filter(|(c, _)| c == api::EXPORT_EVENT).map(|(_, v)| v.clone()).collect();
    assert!(events.iter().all(|e| e["export_id"] == "p1"));
    let phases: Vec<&str> = events.iter().filter_map(|e| e["phase"].as_str()).collect();
    assert_eq!(phases.first(), Some(&"connecting"));
    assert!(phases.contains(&"running") && phases.contains(&"writing"), "{phases:?}");
    assert_eq!(phases.last(), Some(&"finishing"));
    assert_eq!(events.last().unwrap()["rows"], 50000);
    assert_eq!(st.engine.exported_file("p1").as_deref(), Some(path.as_path()));
    assert!(st.engine.exported_file("nope").is_none());
}

/// A full export of a result read inside an open transaction sees the same
/// uncommitted rows (it runs on the tab's session, not a new one).
#[tokio::test(flavor = "multi_thread")]
async fn export_reads_through_the_open_transaction() {
    use databrain_auth::AuthMethod;
    use databrain_connector_core::{ConnectionConfig, ConnectorKind};
    use databrain_workspace::{ConnectionProfile, EnvTag};
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let dir = tempfile::tempdir().unwrap();
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(dir.path().join("tx.db").to_string_lossy().into_owned());
    let profile = ConnectionProfile {
        id: String::new(),
        name: "tx".into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    let conn = api::save_connection(&st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;
    run(&st, &conn, "create table t (a int)", None).await;
    run(&st, &conn, "insert into t values (1)", None).await;
    run(&st, &conn, "begin", None).await;
    run(&st, &conn, "insert into t values (2), (3)", None).await;
    let r = run(&st, &conn, "select a from t order by a", Some(1)).await;
    let info = r.result.unwrap();
    assert!(info.truncated);
    let path = dir.path().join("all.csv");
    let n = api::export_full_result(&st, &info.id, ExportOptions::new(ExportFormat::Csv), path.to_string_lossy().into_owned(), "tx1").await.unwrap();
    assert_eq!(n, 3, "the uncommitted rows are exported too");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n1\n2\n3\n");
    // The transaction is still open on the tab's session.
    run(&st, &conn, "rollback", None).await;
    let after = run(&st, &conn, "select count(*) from t", None).await;
    assert_eq!(after.result.unwrap().total_rows, 1);
}
