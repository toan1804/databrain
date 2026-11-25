//! The table explorer's server side: the notes about a table (with the
//! column each is on, and the target a new note gets) and the check of its
//! sample SQL (the `where` condition typed in must only read).

use std::sync::Arc;

use databrain_app::ai_api;
use databrain_app::api::{self, AppState, SaveConnectionArgs};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::{EventSink, JobEvent};
use databrain_workspace::{ConnectionProfile, EnvTag, KnNote, NoteStatus, Workspace};

struct Quiet;
impl EventSink for Quiet {
    fn emit(&self, _: JobEvent) {}
}

struct NoUi;
impl ai_api::UiBridge for NoUi {
    fn emit(&self, _: &str, _: serde_json::Value) {}
    fn open_url(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

fn state() -> AppState {
    AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi))
}

fn connection(st: &AppState, kind: ConnectorKind) -> String {
    let mut c = ConnectionConfig::new(kind, AuthMethod::None);
    c.file_path = Some("/tmp/never-opened.db".into());
    let profile = ConnectionProfile {
        id: String::new(),
        name: format!("{kind:?}"),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    api::save_connection(st, SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id
}

fn note(st: &AppState, conn: &str, target: &str, body: &str) {
    st.workspace
        .kn_save_note(KnNote {
            id: String::new(),
            connection_id: conn.into(),
            target: Some(target.into()),
            body: body.into(),
            author: "user".into(),
            status: NoteStatus::Approved,
            created_at: 0,
            replaces: None,
        })
        .unwrap();
}

/// A note saved with the target the tab offers comes back for the table,
/// whatever the schema id looks like (a BigQuery project, DuckDB results,
/// an attached file); a note on a column says which; a note on the table
/// isn't on a column that happens to share its name.
#[test]
fn table_notes_come_back_with_their_column() {
    let st = state();
    let conn = connection(&st, ConnectorKind::Duckdb);
    for (schema, name) in [("public", "orders"), ("my-project.sales", "orders"), ("results", "main"), ("data.csv.files", "data.csv"), ("s", "and")] {
        let t = ai_api::table_notes(&st, &conn, schema, name).unwrap();
        note(&st, &conn, &t.target, &format!("about {schema}.{name}"));
        let back = ai_api::table_notes(&st, &conn, schema, name).unwrap();
        assert!(
            back.notes.iter().any(|n| n.note.body == format!("about {schema}.{name}") && n.column.is_none()),
            "{schema}.{name}: {} gave {:?}",
            t.target,
            back.notes.iter().map(|n| &n.note.body).collect::<Vec<_>>()
        );
    }
    note(&st, &conn, "public.orders.total", "incl. VAT");
    note(&st, &conn, "public.customers", "elsewhere");
    let t = ai_api::table_notes(&st, &conn, "public", "orders").unwrap();
    let cols: Vec<(&str, Option<&str>)> = t.notes.iter().map(|n| (n.note.body.as_str(), n.column.as_deref())).collect();
    assert!(cols.contains(&("incl. VAT", Some("total"))), "{cols:?}");
    assert!(cols.contains(&("about public.orders", None)), "{cols:?}");
    assert!(!cols.iter().any(|(b, _)| *b == "elsewhere"), "{cols:?}");
}

/// The sample SQL runs only when it is one read without parameters: a `;`
/// in the condition can't start a write, on any database's splitter.
#[test]
fn sample_sql_must_be_one_read() {
    let st = state();
    for kind in [ConnectorKind::Postgres, ConnectorKind::Mysql, ConnectorKind::Sqlite, ConnectorKind::Mssql] {
        let conn = connection(&st, kind);
        let ok = "SELECT *\nFROM t\nWHERE status = 'a;b'\nLIMIT 100;";
        assert!(ai_api::check_sample_sql(&st, &conn, ok).is_ok(), "{kind:?}");
        for (bad, why) in [
            ("SELECT *\nFROM t\nWHERE 1=1; update t set x = 1 where id > 0\nLIMIT 100;", "one condition"),
            ("SELECT *\nFROM t\nWHERE 1=1; delete from t\nLIMIT 100;", "one condition"),
            ("SELECT *\nFROM t\nWHERE id = :id\nLIMIT 100;", "Parameters"),
        ] {
            let e = ai_api::check_sample_sql(&st, &conn, bad).unwrap_err();
            assert!(e.message.contains(why), "{kind:?} {bad}: {}", e.message);
        }
    }
}

/// The profile, statistics and counts of the table explorer (step 2) run
/// through `explore_query`: one read on the table tab's own session, its
/// rows returned (a profile is one row) and the result dropped, kept in the
/// history as Explorer; a write is refused by the engine, whatever the UI sent.
#[tokio::test(flavor = "multi_thread")]
async fn explorer_reads_return_rows_and_refuse_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("shop.db");
    let st = state();
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(db.to_string_lossy().into());
    let profile = ConnectionProfile {
        id: String::new(),
        name: "Shop".into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    let conn = api::save_connection(&st, SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap().id;
    let setup = databrain_query_engine::RunRequest {
        connection_id: conn.clone(),
        tab_id: "setup".into(),
        sql: "create table orders(id integer primary key, status text, total real); insert into orders(status, total) values ('paid', 10.5), ('paid', 3), ('open', null);".into(),
        base_offset: 0,
        row_limit: None,
        confirmed: true,
        origin: databrain_workspace::Origin::User,
        session_key: None,
        output_name: None,
        params: Default::default(),
        output_masks: None,
        part_rows: None,
    pushdown: None,
    };
    for o in st.engine.run_and_wait(&st.hub, setup, None).await.unwrap() {
        assert!(o.error.is_none(), "{:?}", o.error);
    }

    // The profile the tab builds for SQLite (ui/src/lib/explore.ts).
    let sql = "SELECT\n  COUNT(*) AS total,\n  COUNT(id) AS n0,\n  COUNT(DISTINCT id) AS d0,\n  MIN(id) AS lo0,\n  MAX(id) AS hi0,\n  COUNT(status) AS n1,\n  COUNT(DISTINCT status) AS d1,\n  COUNT(total) AS n2,\n  COUNT(DISTINCT total) AS d2,\n  MIN(total) AS lo2,\n  MAX(total) AS hi2\nFROM orders";
    let rows = api::explore_query(&st, &conn, "tab1#profile", sql).await.unwrap();
    assert_eq!(rows.columns[..3], ["total", "n0", "d0"]);
    let row: Vec<Option<&str>> = rows.rows[0].iter().map(|v| v.as_deref()).collect();
    assert_eq!(row[0], Some("3"));
    assert_eq!((row[5], row[6], row[7], row[8]), (Some("3"), Some("2"), Some("2"), Some("2")), "{row:?}");

    // Top values: one row a value.
    let top = api::explore_query(&st, &conn, "tab1#top", "SELECT status AS value, COUNT(*) AS n\nFROM orders\nGROUP BY status\nORDER BY n DESC\nLIMIT 10")
        .await
        .unwrap();
    assert_eq!(top.rows[0], vec![Some("paid".to_string()), Some("2".to_string())]);

    // A write (or a read followed by one) is refused, and nothing changed.
    for bad in ["DELETE FROM orders", "SELECT 1; DELETE FROM orders"] {
        let e = api::explore_query(&st, &conn, "tab1#count", bad).await.unwrap_err();
        assert!(!e.message.is_empty(), "{bad}");
    }
    let n = api::explore_query(&st, &conn, "tab1#count", "SELECT COUNT(*) AS n\nFROM orders").await.unwrap();
    assert_eq!(n.rows[0][0].as_deref(), Some("3"));

    // Returned, not kept: none of these reads is left in the results list.
    assert!(api::list_outputs(&st).iter().all(|o| !o.tab_id.starts_with("tab1#")), "{:?}", api::list_outputs(&st));

    // In the history as the explorer's.
    let origins: Vec<databrain_workspace::Origin> = st.workspace.list_history(&Default::default()).unwrap().into_iter().map(|h| h.origin).collect();
    assert!(origins.contains(&databrain_workspace::Origin::Explorer), "{origins:?}");
    for k in ["profile", "top", "count"] {
        api::close_tab(&st, &format!("tab1#{k}"));
    }
}
