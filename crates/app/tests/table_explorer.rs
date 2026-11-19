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
