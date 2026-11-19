//! `dbr` (`databrain query`): SQL from a terminal on saved connections,
//! with their rules: production asks (refused without a terminal unless
//! --yes), read-only refuses writes, `:name` values, a table capped at 1,000
//! rows, every row as CSV, and the runs in the history as the terminal's.

use std::sync::Arc;

use databrain_app::api::{self, AppState, SaveConnectionArgs};
use databrain_app::cli::query::{Terminal, parse, run_with};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::{EventSink, JobEvent};
use databrain_workspace::{ConnectionProfile, EnvTag, Origin, Workspace};

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

fn sqlite(st: &AppState, name: &str, path: &std::path::Path, env: EnvTag, read_only: bool) {
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(path.to_string_lossy().into_owned());
    c.read_only = read_only;
    let profile = ConnectionProfile {
        id: String::new(),
        name: name.into(),
        config: c,
        color: None,
        env,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    api::save_connection(st, SaveConnectionArgs { profile, secret: None, clear_secret: false, extra_secrets: Default::default() }).unwrap();
}

/// `dbr <args>`: the exit code and what it printed.
async fn dbr(st: &AppState, args: &[&str], tty: bool) -> (u8, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let opts = parse(&args, |p| p.is_file()).unwrap();
    let mut out = Vec::new();
    let code = run_with(st, opts, &mut out, Terminal { out: tty, input: false }).await;
    (code, String::from_utf8(out).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn dbr_runs_sql_with_each_connections_rules() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    sqlite(&st, "Sales", &dir.path().join("sales.db"), EnvTag::Dev, false);
    sqlite(&st, "Prod", &dir.path().join("prod.db"), EnvTag::Prod, false);
    // Read-only, on the same file as Sales (a read-only connection doesn't create its file).
    sqlite(&st, "Archive", &dir.path().join("sales.db"), EnvTag::None, true);

    // A table in the terminal: the rows, numbers to the right, the footer.
    let (code, _) = dbr(&st, &["-c", "sales", "-i", "create table t (id integer, name text); insert into t values (1, 'a'), (2, null)"], true).await;
    assert_eq!(code, 0);
    let (code, out) = dbr(&st, &["-c", "Sales", "-i", "select id, name from t order by id"], true).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("id │ name") && out.contains(" 2 │ NULL") && out.contains("2 rows · Sales"), "{out}");
    // :name values.
    let (code, out) = dbr(&st, &["-c", "Sales", "-i", "-p", "id=2", "--csv", "select id from t where id = :id"], true).await;
    assert_eq!((code, out.as_str()), (0, "id\n2\n"));

    // Production: a write asks; without a terminal it is refused unless --yes.
    let (code, _) = dbr(&st, &["-c", "Prod", "-i", "--yes", "create table p (id integer)"], false).await;
    assert_eq!(code, 0);
    let (code, _) = dbr(&st, &["-c", "Prod", "-i", "insert into p values (1)"], false).await;
    assert_eq!(code, 2, "refused without a terminal");
    let (_, out) = dbr(&st, &["-c", "Prod", "-i", "--csv", "select count(*) as n from p"], false).await;
    assert_eq!(out, "n\n0\n", "nothing written");
    let (code, _) = dbr(&st, &["-c", "Prod", "-i", "--yes", "insert into p values (1)"], false).await;
    assert_eq!(code, 0);
    let (_, out) = dbr(&st, &["-c", "Prod", "-i", "--csv", "select count(*) as n from p"], false).await;
    assert_eq!(out, "n\n1\n");

    // Read-only refuses the write (a failed statement: exit 1), --yes or not.
    let (code, _) = dbr(&st, &["-c", "Archive", "-i", "--yes", "create table x (id integer)"], false).await;
    assert_ne!(code, 0);
    let (code, out) = dbr(&st, &["-c", "Archive", "-i", "--csv", "select count(*) as n from sqlite_master where name = 'x'"], false).await;
    assert_eq!((code, out.as_str()), (0, "n\n0\n"), "reads are fine, and nothing was created");

    // A table shows at most 1,000 rows; CSV every row.
    let many = "with recursive n(i) as (select 1 union all select i + 1 from n where i < 1500) select i from n";
    let (code, out) = dbr(&st, &["-c", "Sales", "-i", "--limit", "1000", many], true).await;
    assert_eq!(code, 0);
    assert!(out.contains("1000 rows (more not read"), "{}", out.lines().last().unwrap_or(""));
    let (code, _) = dbr(&st, &["-c", "Sales", "-i", "--limit", "1001", many], true).await;
    assert_eq!(code, 2);
    let (code, out) = dbr(&st, &["-c", "Sales", "-i", "--csv", many], true).await;
    assert_eq!((code, out.lines().count()), (0, 1501));

    // A failed statement: exit 1.
    let (code, _) = dbr(&st, &["-c", "Sales", "-i", "select * from nope"], false).await;
    assert_eq!(code, 1);

    // In the history as the terminal's.
    let origins: Vec<Origin> = st.workspace.list_history(&Default::default()).unwrap().into_iter().map(|h| h.origin).collect();
    assert!(origins.contains(&Origin::Cli), "{origins:?}");
}
