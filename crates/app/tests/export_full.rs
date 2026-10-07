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
