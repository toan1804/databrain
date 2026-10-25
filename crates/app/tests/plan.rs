//! Query plans read from SQLite and DuckDB (the engines that run without a server).

use std::sync::Arc;

use databrain_app::api::{self, AppState};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::plan::PlanNode;
use databrain_query_engine::{EventSink, JobEvent, RunRequest};
use databrain_workspace::{ConnectionProfile, EnvTag, Workspace};

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

fn conn(st: &AppState, kind: ConnectorKind, path: &std::path::Path) -> String {
    let mut c = ConnectionConfig::new(kind, AuthMethod::None);
    c.file_path = Some(path.to_string_lossy().into_owned());
    let profile = ConnectionProfile { id: String::new(), name: format!("{kind:?}"), config: c, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 };
    api::save_connection(st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id
}

async fn run(st: &AppState, conn: &str, sql: &str) {
    let out = st
        .engine
        .run_and_wait(&st.hub, RunRequest { connection_id: conn.into(), tab_id: "tab".into(), sql: sql.into(), base_offset: 0, row_limit: None, confirmed: true, origin: Default::default(), session_key: None, output_name: None, params: Default::default() }, None)
        .await
        .unwrap();
    assert!(out.iter().all(|o| o.error.is_none()), "{sql}: {:?}", out.iter().find_map(|o| o.error.clone()));
}

fn all_ops(nodes: &[PlanNode]) -> Vec<String> {
    nodes.iter().flat_map(|n| std::iter::once(n.op.clone()).chain(all_ops(&n.children))).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_and_duckdb_plans() {
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let dir = tempfile::tempdir().unwrap();
    let none = Default::default();

    let lite = conn(&st, ConnectorKind::Sqlite, &dir.path().join("p.db"));
    run(&st, &lite, "create table a (id integer primary key, v text); create table b (a_id integer, w text); create index b_a on b(a_id)").await;
    let p = api::explain_plan(&st, &lite, "tab", "select * from a join b on b.a_id = a.id where a.v = :v", false, &[("v".to_string(), databrain_connector_core::sql::ParamValue { value: "x".into(), raw: false })].into()).await.unwrap();
    let ops = all_ops(&p.roots).join(" | ");
    assert!(ops.contains("SCAN a") && ops.contains("b_a"), "{ops}");
    assert!(api::explain_plan(&st, &lite, "tab", "select 1", true, &none).await.unwrap_err().message.contains("analyzed"));
    assert!(api::explain_plan(&st, &lite, "tab", "select :missing", false, &none).await.unwrap_err().message.contains(":missing"));

    let duck = conn(&st, ConnectorKind::Duckdb, &dir.path().join("p.duckdb"));
    run(&st, &duck, "create table t as select range as id, range % 7 as g from range(1000)").await;
    let p = api::explain_plan(&st, &duck, "tab", "select g, count(*) from t where id > 10 group by g", false, &none).await.unwrap();
    assert!(!p.roots.is_empty(), "{}", p.raw);
    let ops = all_ops(&p.roots).join(" | ");
    assert!(ops.to_uppercase().contains("SCAN"), "{ops}");
    let a = api::explain_plan(&st, &duck, "tab", "select g, count(*) from t group by g", true, &none).await.unwrap();
    assert!(a.analyzed && !a.roots.is_empty(), "{}", a.raw);
    fn any_actual(n: &[PlanNode]) -> bool {
        n.iter().any(|x| x.actual_rows.is_some() || any_actual(&x.children))
    }
    assert!(any_actual(&a.roots), "{}", a.raw);
    // ANALYZE runs the statement: writes are refused.
    assert!(api::explain_plan(&st, &duck, "tab", "delete from t", true, &none).await.unwrap_err().message.contains("read queries"));
}

/// Live: Postgres JSON and MySQL tree plans (`DATABRAIN_PG_*`, `DATABRAIN_MYSQL_*`).
#[tokio::test(flavor = "multi_thread")]
async fn live_server_plans() {
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let none = Default::default();
    let server = |kind: ConnectorKind, prefix: &str, user: &str| -> Option<String> {
        let host = std::env::var(format!("DATABRAIN_{prefix}_HOST")).ok()?;
        let user = std::env::var(format!("DATABRAIN_{prefix}_USER")).unwrap_or_else(|_| user.into());
        let mut c = ConnectionConfig::new(kind, AuthMethod::Password { user });
        c.host = Some(host);
        c.port = std::env::var(format!("DATABRAIN_{prefix}_PORT")).ok().and_then(|p| p.parse().ok());
        c.database = std::env::var(format!("DATABRAIN_{prefix}_DB")).ok();
        let profile = ConnectionProfile { id: String::new(), name: prefix.into(), config: c, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 };
        let secret = std::env::var(format!("DATABRAIN_{prefix}_PASSWORD")).ok();
        Some(api::save_connection(&st, api::SaveConnectionArgs { profile, secret, clear_secret: false, extra_secrets: Default::default() }).unwrap().id)
    };
    if let Some(pg) = server(ConnectorKind::Postgres, "PG", "postgres") {
        let p = api::explain_plan(&st, &pg, "tab", "select c.relname, count(*) from pg_class c join pg_namespace n on n.oid = c.relnamespace group by c.relname", true, &none).await.unwrap();
        assert!(p.analyzed && p.execution_ms.is_some() && p.roots[0].time_ms.is_some(), "{}", p.raw);
        assert!(all_ops(&p.roots).iter().any(|o| o.contains("Join") || o.contains("Nested Loop")), "{:?}", all_ops(&p.roots));
    } else {
        eprintln!("skipping Postgres: DATABRAIN_PG_HOST not set");
    }
    if let Some(my) = server(ConnectorKind::Mysql, "MYSQL", "root") {
        let sql = "select t.table_name, count(*) from information_schema.tables t join information_schema.columns c on c.table_name = t.table_name group by t.table_name";
        let p = api::explain_plan(&st, &my, "tab", sql, false, &none).await.unwrap();
        assert!(!p.roots.is_empty(), "{}", p.raw);
        let a = api::explain_plan(&st, &my, "tab", sql, true, &none).await.unwrap();
        fn any_time(n: &[PlanNode]) -> bool {
            n.iter().any(|x| x.time_ms.is_some() || any_time(&x.children))
        }
        assert!(any_time(&a.roots), "{}", a.raw);
    } else {
        eprintln!("skipping MySQL: DATABRAIN_MYSQL_HOST not set");
    }
}
