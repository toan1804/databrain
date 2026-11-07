use super::*;
use crate::migrations::MIGRATIONS;
use databrain_auth::AuthMethod;
use databrain_connector_core::ConnectionConfig;
use databrain_connector_core::ConnectorKind;
use rusqlite::params;

fn profile(name: &str) -> ConnectionProfile {
    ConnectionProfile {
        id: String::new(),
        name: name.into(),
        config: {
            let mut c = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: "me".into() });
            c.host = Some("db.local".into());
            c.port = Some(5432);
            c.database = Some("app".into());
            c
        },
        color: Some("#ef4444".into()),
        env: EnvTag::Prod,
        folder_id: None,
        has_secret: true,
        ai_policy: AiPolicy::default(),
        created_at: 0,
        updated_at: 0,
    }
}

/// Password forms the SQL-only redaction kept in clear: what history and
/// job logs store now matches the AI audit log, and older rows are
/// cleaned once.
const SECRET_FORMS: [&str; 7] = [
    "ATTACH 'postgres://u:pw1@h/db' AS p (TYPE postgres)",
    "CHANGE MASTER TO MASTER_USER='r', MASTER_PASSWORD='pw2'",
    "db.createUser({user: \"app\", pwd: \"pw3\", roles: []})",
    "EXEC sp_addlinkedsrvlogin @rmtsrvname = 'S', @rmtuser = 'u', @rmtpassword = 'pw4'",
    "select dblink_connect('host=h user=u password=pw5')",
    "select * from openrowset('MSDASQL', 'Driver={SQL Server};UID=u;PWD=pw6;', 'select 1')",
    "load 'https://h/x?user=u&password=pw7'",
];

fn clear(text: &str) -> bool {
    (1..=7).any(|i| text.contains(&format!("pw{i}")))
}

#[test]
fn history_and_job_logs_redact_like_the_audit_log() {
    let ws = Workspace::open_in_memory().unwrap();
    for sql in SECRET_FORMS {
        ws.add_history(NewHistory { sql: sql.into(), error: Some(format!("failed: {sql}")), ..Default::default() }).unwrap();
        assert!(!clear(&crate::jobs::log_sql(sql)), "job log: {sql}");
    }
    let kept = ws.list_history(&HistoryQuery { limit: Some(50), ..Default::default() }).unwrap();
    assert_eq!(kept.len(), 7);
    for h in kept {
        assert!(!clear(&h.sql) && !h.error.as_deref().is_some_and(clear), "{} / {:?}", h.sql, h.error);
        assert!(h.sql.contains("***"), "{}", h.sql);
    }
}

#[test]
fn redacts_kept_history_again_with_the_full_redaction() {
    let dir = std::env::temp_dir().join(format!("databrain-ws-redact-sql-{}", crate::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ws.db");
    {
        let ws = Workspace::open(&path).unwrap();
        let c = ws.conn.lock();
        // As the v1 pass left them.
        for sql in SECRET_FORMS {
            c.execute("INSERT INTO query_history (sql, started_at, duration_ms, status) VALUES (?1, 0, 0, 'success')", [sql]).unwrap();
        }
        c.execute("DELETE FROM maintenance WHERE name = 'secrets_redacted_v2'", []).unwrap();
    }
    let ws = Workspace::open(&path).unwrap();
    let kept: Vec<String> = ws.conn.lock().prepare("SELECT sql FROM query_history").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(kept.len(), 7);
    assert!(kept.iter().all(|s| !clear(s)), "{kept:?}");
    drop(ws);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn connections_crud() {
    let ws = Workspace::open_in_memory().unwrap();
    let saved = ws.save_connection(profile("Prod DB")).unwrap();
    assert!(!saved.id.is_empty());
    assert_eq!(saved.env, EnvTag::Prod);
    assert_eq!(saved.ai_policy, AiPolicy::default());
    assert_eq!(saved.config.host.as_deref(), Some("db.local"));
    let mut edited = saved.clone();
    edited.name = "Production".into();
    ws.save_connection(edited).unwrap();
    let all = ws.list_connections().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].name, "Production");
    assert_eq!(all[0].created_at, saved.created_at);
    ws.delete_connection(&saved.id).unwrap();
    assert!(ws.list_connections().unwrap().is_empty());
    assert!(ws.save_connection(profile("  ")).is_err());
}

#[test]
fn saved_queries_and_folders() {
    let ws = Workspace::open_in_memory().unwrap();
    let f = ws.save_folder(Folder { id: String::new(), parent_id: None, name: "Reports".into(), kind: FolderKind::Queries }).unwrap();
    let q = ws
        .save_query(SavedQuery {
            id: String::new(),
            name: "Monthly revenue".into(),
            sql: "select sum(amount) from orders".into(),
            connection_id: None,
            folder_id: Some(f.id.clone()),
            description: None,
            tags: vec!["finance".into(), " ".into()],
            ai_example: true,
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    assert_eq!(q.tags, vec!["finance"]);
    assert_eq!(ws.list_saved_queries(Some("orders")).unwrap().len(), 1);
    assert_eq!(ws.list_saved_queries(Some("finance")).unwrap().len(), 1);
    assert_eq!(ws.list_saved_queries(Some("100%")).unwrap().len(), 0);
    assert!(ws.get_saved_query(&q.id).unwrap().ai_example);
    ws.move_to_folder(FolderKind::Queries, &q.id, None).unwrap();
    assert_eq!(ws.get_saved_query(&q.id).unwrap().folder_id, None);
    ws.move_to_folder(FolderKind::Queries, &q.id, Some(&f.id)).unwrap();
    ws.delete_folder(&f.id).unwrap();
    assert_eq!(ws.get_saved_query(&q.id).unwrap().folder_id, None);
    ws.delete_saved_query(&q.id).unwrap();
    assert!(ws.list_saved_queries(None).unwrap().is_empty());
}

/// History search: every word, in any order, in the SQL, the error or
/// the connection name; `"…"` is one phrase; filters by connection and
/// by how the run ended.
/// A workspace from a newer DataBrain is refused, untouched; one from an
/// older version is backed up before its schema changes.
#[test]
fn schema_version_guard_and_backup() {
    let dir = std::env::temp_dir().join(format!("dbtest-ws-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("workspace.db");
    drop(Workspace::open(&path).unwrap());
    // Newer: refused, version unchanged.
    {
        let c = Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", (MIGRATIONS.len() + 3) as i64).unwrap();
    }
    let e = Workspace::open(&path).err().expect("refused");
    assert!(e.to_string().contains("newer DataBrain"), "{e}");
    let v: i64 = Connection::open(&path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v as usize, MIGRATIONS.len() + 3);
    // Older (one migration behind): backed up as it was, then updated.
    {
        let c = Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", (MIGRATIONS.len() - 1) as i64).unwrap();
        // Undo the last migration, as an older version left it.
        c.execute_batch("DROP TABLE kn_join_seen; DROP TABLE kn_joins; DROP TABLE kn_table_seen; DROP TABLE kn_learn_state").unwrap();
    }
    let ws = Workspace::open(&path).unwrap();
    assert!(ws.learned_joins("any").unwrap().is_empty(), "migrated");
    let backup = dir.join(format!("workspace.db.bak-before-v{}", MIGRATIONS.len()));
    let b = Connection::open(&backup).unwrap();
    let bv: i64 = b.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(bv as usize, MIGRATIONS.len() - 1, "the backup is the workspace before the update");
    drop((ws, b));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_search_words_and_filters() {
    let ws = Workspace::open_in_memory().unwrap();
    let sales = ws.save_connection(profile("Sales warehouse")).unwrap();
    let crm = ws.save_connection(profile("CRM")).unwrap();
    let add = |conn: &str, sql: &str, error: Option<&str>| {
        ws.add_history(NewHistory {
            origin: Origin::User,
            connection_id: Some(conn.into()),
            sql: sql.into(),
            started_at: now_ms(),
            duration_ms: 1,
            rows: None,
            status: Some(if error.is_some() { RunStatus::Error } else { RunStatus::Success }),
            error: error.map(str::to_string),
            output_handle: None,
            result_id: None,
        })
        .unwrap();
    };
    add(&sales.id, "select status, sum(amount) from orders where created_at >= '2026-01-01' group by status", None);
    add(&sales.id, "select * from order_items", Some("relation \"order_items\" does not exist"));
    add(&crm.id, "select name from customers where country = 'VN'", None);
    let find = |search: &str, f: &dyn Fn(&mut HistoryQuery)| {
        let mut q = HistoryQuery { search: Some(search.into()), ..Default::default() };
        f(&mut q);
        ws.list_history(&q).unwrap().into_iter().map(|h| h.sql.split_whitespace().last().unwrap().to_string()).collect::<Vec<_>>()
    };
    let none = |_: &mut HistoryQuery| {};
    assert_eq!(find("2026 SUM orders", &none), ["status"], "any order, any case");
    assert_eq!(find("orders customers", &none), Vec::<String>::new(), "every word must match");
    assert_eq!(find("does not exist", &none), ["order_items"], "the error text");
    assert_eq!(find("warehouse", &none).len(), 2, "the connection name");
    assert_eq!(find("\"from orders\"", &none), ["status"], "a phrase");
    assert_eq!(find("100%", &none), Vec::<String>::new(), "% is a character, not a wildcard");
    assert_eq!(find("select", &|q| q.connection_id = Some(crm.id.clone())), ["'VN'"]);
    assert_eq!(find("", &|q| q.status = Some(RunStatus::Error)), ["order_items"]);
    assert_eq!(history_terms(r#"  a "b c"  d "#), ["a", "b c", "d"]);
}

#[test]
fn history_tabs_settings() {
    let ws = Workspace::open_in_memory().unwrap();
    let conn = ws.save_connection(profile("A")).unwrap();
    for i in 0..3 {
        ws.add_history(NewHistory {
            origin: if i == 1 { Origin::Ai } else { Origin::User },
            connection_id: Some(conn.id.clone()),
            sql: format!("select {i}"),
            started_at: now_ms(),
            duration_ms: 5,
            rows: Some(1),
            status: Some(if i == 2 { RunStatus::Error } else { RunStatus::Success }),
            error: None,
            output_handle: (i == 0).then(|| "r1".to_string()),
            result_id: (i == 0).then(|| "job:0".to_string()),
        })
        .unwrap();
    }
    let h = ws.list_history(&HistoryQuery::default()).unwrap();
    assert_eq!(h.last().unwrap().output_handle.as_deref(), Some("r1"));
    assert_eq!(h.len(), 3);
    assert_eq!(h[0].sql, "select 2");
    assert_eq!(h[0].status, RunStatus::Error);
    assert_eq!(h[0].connection_name.as_deref(), Some("A"));
    assert_eq!(h[1].origin, Origin::Ai);
    let mine = ws.list_history(&HistoryQuery { exclude_ai: true, ..Default::default() }).unwrap();
    assert_eq!(mine.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["select 2", "select 0"]);
    let h = ws.list_history(&HistoryQuery { search: Some("1".into()), ..Default::default() }).unwrap();
    assert_eq!(h.len(), 1);
    let page = ws.list_history(&HistoryQuery { before: Some(h[0].id), ..Default::default() }).unwrap();
    assert_eq!(page.len(), 1);
    // Clearing: AI only, then by age (retention), keeping recent entries.
    {
        let old = now_ms() - 40 * 86_400_000;
        ws.conn.lock().execute("UPDATE query_history SET started_at = ?1 WHERE sql = 'select 0'", params![old]).unwrap();
    }
    assert_eq!(ws.history_retention_days(), Some(30), "default: one month");
    ws.set_setting(HISTORY_RETENTION_SETTING, &serde_json::json!(0)).unwrap();
    assert_eq!((ws.history_retention_days(), ws.prune_history().unwrap().0), (None, 0), "0 = keep everything");
    ws.set_setting(HISTORY_RETENTION_SETTING, &serde_json::json!(30)).unwrap();
    assert_eq!(ws.clear_history_by(HistoryClear::Ai).unwrap(), 1);
    assert_eq!(ws.prune_history().unwrap(), (1, vec!["job:0".to_string()]), "the 40-day-old entry, with its result");
    assert_eq!(ws.list_history(&HistoryQuery::default()).unwrap().iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["select 2"]);
    let last = ws.list_history(&HistoryQuery::default()).unwrap()[0].id;
    assert_eq!(ws.delete_history_entry(last).unwrap(), None);
    assert!(ws.list_history(&HistoryQuery::default()).unwrap().is_empty());
    ws.conn.lock().execute("DELETE FROM settings WHERE key = ?1", params![HISTORY_RETENTION_SETTING]).unwrap();

    let tabs = vec![
        TabState {
            id: "t1".into(),
            title: "Query 1".into(),
            sql: "select 1".into(),
            connection_id: None,
            saved_query_id: None,
            notebook_id: None,
            output_ref: None,
            auto_commit: None,
            file_path: None,
        },
        TabState {
            id: "t2".into(),
            title: "Query 2".into(),
            sql: "".into(),
            connection_id: Some(conn.id.clone()),
            saved_query_id: None,
            notebook_id: None,
            output_ref: None,
            auto_commit: Some(false),
            file_path: Some("/tmp/q.sql".into()),
        },
    ];
    ws.save_tabs(&tabs).unwrap();
    assert_eq!(ws.list_tabs().unwrap(), tabs);
    ws.save_tabs(&tabs[1..]).unwrap();
    assert_eq!(ws.list_tabs().unwrap().len(), 1);

    ws.set_setting("theme", &serde_json::json!("dark")).unwrap();
    assert_eq!(ws.get_setting("theme").unwrap(), Some(serde_json::json!("dark")));
    assert_eq!(ws.all_settings().unwrap().len(), 1);
}

#[test]
fn reopen_keeps_data() {
    let path = std::env::temp_dir().join(format!("databrain-ws-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let ws = Workspace::open(&path).unwrap();
        ws.save_connection(profile("keep")).unwrap();
    }
    let ws = Workspace::open(&path).unwrap();
    assert_eq!(ws.list_connections().unwrap()[0].name, "keep");
    drop(ws);
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}

#[test]
fn notes_export_import_round_trip() {
    use crate::knowledge::{ImportAction, ImportKind};
    let ws = Workspace::open_in_memory().unwrap();
    let a = ws.save_connection(profile("A")).unwrap();
    let b = ws.save_connection(profile("B")).unwrap();
    let note = |conn: &str, target: Option<&str>, body: &str| KnNote {
        id: String::new(),
        connection_id: conn.into(),
        target: target.map(String::from),
        body: body.into(),
        author: "user".into(),
        status: NoteStatus::Approved,
        created_at: 0,
        replaces: None,
    };
    ws.kn_save_note(note(&a.id, Some("public.orders"), "Orders with status 9 are test orders.")).unwrap();
    ws.kn_save_note(note(&a.id, None, "Active customer: has an order in the last 90 days")).unwrap();
    ws.kn_save_note(note(&a.id, None, "MRR = monthly recurring revenue")).unwrap();
    ws.kn_save_note(KnNote { status: NoteStatus::Proposed, ..note(&a.id, None, "pending, not exported") }).unwrap();
    // B already knows some of it.
    ws.kn_save_note(note(&b.id, None, "MRR = monthly recurring revenue")).unwrap();
    let old = ws.kn_save_note(note(&b.id, None, "Active customer: logged in this month")).unwrap();

    let file = ws.kn_export_notes(&a.id).unwrap();
    assert_eq!(file.notes.len(), 3, "approved notes only");
    let text = serde_json::to_string(&file).unwrap();
    let file: NotesFile = serde_json::from_str(&text).unwrap();
    assert_eq!(file.source.as_ref().unwrap().connection, "A");

    let plan = ws.kn_import_plan(&b.id, &file).unwrap();
    let kind = |body: &str| plan.iter().find(|p| p.incoming.body.starts_with(body)).unwrap();
    assert_eq!(kind("Orders").kind, ImportKind::New);
    assert_eq!(kind("MRR").kind, ImportKind::Same);
    let conflict = kind("Active");
    assert_eq!(conflict.kind, ImportKind::Conflict, "same glossary term, different text");
    assert_eq!(conflict.existing[0].id, old.id);

    let acts = vec![
        ImportAction { incoming: kind("Orders").incoming.clone(), action: "add".into(), existing_ids: vec![], body: None, target: None },
        ImportAction {
            incoming: conflict.incoming.clone(),
            action: "merge".into(),
            existing_ids: vec![old.id.clone()],
            body: Some("Active customer: logged in this month and has an order in the last 90 days".into()),
            target: None,
        },
    ];
    assert_eq!(ws.kn_import_apply(&b.id, &acts).unwrap(), 2);
    let notes = ws.kn_notes(&b.id).unwrap();
    assert_eq!(notes.len(), 3);
    assert!(notes.iter().all(|n| n.id != old.id), "merged note replaced the old one");
    assert!(notes.iter().any(|n| n.target.as_deref() == Some("public.orders")));
    assert!(ws.kn_import_plan(&b.id, &NotesFile { format: "x".into(), ..file.clone() }).is_err());
    // Importing again: nothing new.
    assert!(ws.kn_import_plan(&b.id, &ws.kn_export_notes(&b.id).unwrap()).unwrap().iter().all(|p| p.kind == ImportKind::Same));
}

#[test]
fn approving_an_update_replaces_the_note() {
    let ws = Workspace::open_in_memory().unwrap();
    let a = ws.save_connection(profile("A")).unwrap();
    let base = KnNote {
        id: String::new(),
        connection_id: a.id.clone(),
        target: None,
        body: "status 2 = closed".into(),
        author: "user".into(),
        status: NoteStatus::Approved,
        created_at: 0,
        replaces: None,
    };
    let old = ws.kn_save_note(base.clone()).unwrap();
    let upd = ws
        .kn_save_note(KnNote {
            body: "status 2 = closed, 3 = archived".into(),
            author: "ai".into(),
            status: NoteStatus::Proposed,
            replaces: Some(old.id.clone()),
            ..base
        })
        .unwrap();
    assert_eq!(ws.kn_notes(&a.id).unwrap().len(), 2, "old note kept while the update is pending");
    assert_eq!(ws.kn_notes(&a.id).unwrap().iter().find(|n| n.id == upd.id).unwrap().replaces.as_deref(), Some(old.id.as_str()));
    ws.kn_save_note(KnNote { status: NoteStatus::Approved, ..upd }).unwrap();
    let notes = ws.kn_notes(&a.id).unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].body.contains("archived") && notes[0].replaces.is_none());
}
