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
        .run_and_wait(&st.hub, RunRequest { connection_id: conn.into(), tab_id: "t".into(), sql: sql.into(), base_offset: 0, row_limit: None, confirmed: true, origin: Default::default(), session_key: None, output_name: None, params: Default::default() }, None)
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
    let profile = ConnectionProfile { id: String::new(), name: "target".into(), config: c, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 };
    let target = api::save_connection(&st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;

    let r = run(&st, &duck, "select i as id, 'name ' || i as name from range(25) t(i)").await;
    let info = r.result.unwrap();
    let args = |mode: LoadMode| LoadResultArgs { target_connection_id: target.clone(), table: "people".into(), mode, key_columns: vec!["id".into()], create_table: true };

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
