//! Editing a result in the grid: pending changes become UPDATE / INSERT /
//! DELETE on the source table, applied all or nothing.

use std::sync::Arc;

use databrain_app::api::{self, AppState};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::edit::{CellValue, RowChange};
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

async fn run(st: &AppState, conn: &str, sql: &str) -> databrain_query_engine::StatementOutcomeView {
    let out = st
        .engine
        .run_and_wait(&st.hub, RunRequest { connection_id: conn.into(), tab_id: "t".into(), sql: sql.into(), base_offset: 0, row_limit: None, confirmed: true, origin: Default::default(), session_key: None, output_name: None, params: Default::default() }, None)
        .await
        .unwrap();
    out.into_iter().next_back().unwrap()
}

async fn rows(st: &AppState, conn: &str, sql: &str) -> Vec<Vec<Option<String>>> {
    let r = run(st, conn, sql).await;
    let id = r.result.unwrap_or_else(|| panic!("{sql}: {:?}", r.error)).id;
    api::fetch_page(st, id, Default::default(), 0, 100).await.unwrap().rows
}

fn v(column: usize, value: Option<&str>) -> CellValue {
    CellValue { column, value: value.map(Into::into) }
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_rows_of_a_single_table_result() {
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let dir = tempfile::tempdir().unwrap();
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(dir.path().join("e.db").to_string_lossy().into_owned());
    let profile = ConnectionProfile { id: String::new(), name: "e".into(), config: c, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 };
    let conn = api::save_connection(&st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;
    run(&st, &conn, "create table people (id integer primary key, name text, age integer, active boolean)").await;
    run(&st, &conn, "insert into people values (1, 'Ann', 30, 1), (2, 'Bob', 40, 0), (3, 'Cy', 50, 1)").await;

    let r = run(&st, &conn, "select id, name, age * 2 as double_age, age from people order by id").await;
    let info = r.result.unwrap();
    let t = api::edit_target(&st, &info.id).await.unwrap();
    assert_eq!((t.table.as_str(), t.key_columns.clone(), t.apply.as_str()), ("people", vec![0], "transaction"));
    assert_eq!(t.columns, vec![Some("id".into()), Some("name".into()), None, Some("age".into())], "expressions stay read-only");

    let changes = vec![
        RowChange::Update { key: vec![Some("1".into())], values: vec![v(1, Some("Ann O'Neil")), v(3, None)] },
        RowChange::Insert { values: vec![v(0, Some("4")), v(1, Some("Dee"))] },
        RowChange::Delete { key: vec![Some("2".into())] },
    ];
    let sql: Vec<String> = api::edit_preview(&st, &info.id, &t, &changes).unwrap().into_iter().map(|s| s.sql).collect();
    assert_eq!(
        sql,
        vec![
            r#"UPDATE "main"."people" SET "name" = 'Ann O''Neil', "age" = NULL WHERE "id" = 1"#,
            r#"INSERT INTO "main"."people" ("id", "name") VALUES (4, 'Dee')"#,
            r#"DELETE FROM "main"."people" WHERE "id" = 2"#,
        ]
    );
    // Typed text never becomes SQL: a "number" that isn't one is refused.
    let bad = vec![RowChange::Update { key: vec![Some("1".into())], values: vec![v(3, Some("1; drop table people"))] }];
    assert!(api::edit_preview(&st, &info.id, &t, &bad).unwrap_err().message.contains("not a number"));
    assert!(api::edit_preview(&st, &info.id, &t, &[RowChange::Update { key: vec![Some("1".into())], values: vec![v(2, Some("9"))] }]).is_err(), "read-only column");

    let rep = api::apply_edits(&st, &info.id, changes).await.unwrap();
    assert_eq!((rep.statements, rep.rows, rep.in_open_transaction), (3, 3, false));
    assert_eq!(
        rows(&st, &conn, "select id, name, age from people order by id").await,
        vec![
            vec![Some("1".into()), Some("Ann O'Neil".into()), None],
            vec![Some("3".into()), Some("Cy".into()), Some("50".into())],
            vec![Some("4".into()), Some("Dee".into()), None],
        ]
    );

    // A row deleted meanwhile: nothing is saved (all or nothing).
    let stale = vec![
        RowChange::Update { key: vec![Some("3".into())], values: vec![v(1, Some("Cyrus"))] },
        RowChange::Update { key: vec![Some("2".into())], values: vec![v(1, Some("Ghost"))] },
    ];
    let e = api::apply_edits(&st, &info.id, stale).await.unwrap_err();
    assert!(e.message.contains("changed or deleted"), "{}", e.message);
    assert_eq!(rows(&st, &conn, "select name from people where id = 3").await[0][0].as_deref(), Some("Cy"), "rolled back");

    // Inside the user's open transaction: the changes join it.
    run(&st, &conn, "begin").await;
    let rep = api::apply_edits(&st, &info.id, vec![RowChange::Update { key: vec![Some("3".into())], values: vec![v(1, Some("Cyrus"))] }]).await.unwrap();
    assert!(rep.in_open_transaction);
    run(&st, &conn, "rollback").await;
    assert_eq!(rows(&st, &conn, "select name from people where id = 3").await[0][0].as_deref(), Some("Cy"));

    // Not editable: joins, no key in the result, no primary key.
    let j = run(&st, &conn, "select p.name from people p join people q on p.id = q.id").await.result.unwrap();
    assert!(api::edit_target(&st, &j.id).await.unwrap_err().message.contains("one table"));
    let nk = run(&st, &conn, "select name from people").await.result.unwrap();
    assert!(api::edit_target(&st, &nk.id).await.unwrap_err().message.contains("key column id"));
    run(&st, &conn, "create table logs (msg text)").await;
    let np = run(&st, &conn, "select * from logs").await.result.unwrap();
    assert!(api::edit_target(&st, &np.id).await.unwrap_err().message.contains("no primary key"));
    // A view says so, not "not found" or "no primary key".
    run(&st, &conn, "create view adults as select * from people where age >= 18").await;
    let vw = run(&st, &conn, "select * from adults").await.result.unwrap();
    let e = api::edit_target(&st, &vw.id).await.unwrap_err().message;
    assert_eq!(e, "main.adults is a view: only tables can be edited");
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_foreign_keys_both_ways() {
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    let dir = tempfile::tempdir().unwrap();
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(dir.path().join("fk.db").to_string_lossy().into_owned());
    let profile = ConnectionProfile { id: String::new(), name: "fk".into(), config: c, color: None, env: EnvTag::None, folder_id: None, has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 };
    let conn = api::save_connection(&st, api::SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;
    run(&st, &conn, "create table customers (id integer primary key, name text)").await;
    run(&st, &conn, "create table orders (id integer primary key, customer_id integer references customers(id), amount real)").await;
    run(&st, &conn, "insert into customers values (1, 'Ann'), (2, 'Bob')").await;
    run(&st, &conn, "insert into orders values (10, 1, 5.0), (11, 1, 7.5), (12, 2, 1.0)").await;

    use databrain_query_engine::related::LinkDirection;
    // Orders → their customer.
    let o = run(&st, &conn, "select * from orders order by id").await.result.unwrap();
    let links = api::result_links(&st, &o.id).await.unwrap();
    let parent = links.iter().find(|l| l.direction == LinkDirection::Parent).expect("orders.customer_id → customers");
    assert_eq!((parent.table.as_str(), parent.columns.clone(), parent.target_columns.clone()), ("customers", vec![1], vec!["id".to_string()]));
    let sql = api::related_rows_sql(&st, &o.id, parent, &[vec![Some("1".into())], vec![Some("1".into())]]).unwrap();
    assert_eq!(rows(&st, &conn, &sql).await, vec![vec![Some("1".into()), Some("Ann".into())]]);

    // Customers ← their orders (the schema is read: no knowledge index here).
    let cu = run(&st, &conn, "select id, name from customers").await.result.unwrap();
    let links = api::result_links(&st, &cu.id).await.unwrap();
    let children = links.iter().find(|l| l.direction == LinkDirection::Children).expect("orders reference customers");
    assert_eq!((children.table.as_str(), children.columns.clone(), children.target_columns.clone()), ("orders", vec![0], vec!["customer_id".to_string()]));
    let sql = api::related_rows_sql(&st, &cu.id, children, &[vec![Some("1".into())]]).unwrap();
    assert_eq!(rows(&st, &conn, &format!("{sql} order by id")).await.len(), 2);

    // Not from one table: no links (and no error).
    let j = run(&st, &conn, "select count(*) from orders").await.result.unwrap();
    assert!(api::result_links(&st, &j.id).await.unwrap().is_empty());
}
