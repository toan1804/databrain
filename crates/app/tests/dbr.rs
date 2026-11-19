//! `dbr` (`databrain query`): SQL from a terminal on saved connections,
//! with their rules: production asks (refused without a terminal unless
//! --yes), read-only refuses writes, `:name` values, a table capped at 1,000
//! rows, every row as CSV, and the runs in the history as the terminal's.

use std::sync::Arc;

use databrain_app::api::{self, AppState, SaveConnectionArgs};
use databrain_app::cli::query::{Answer, Io, Terminal, parse, run_with};
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

/// `dbr <args>` with `answer` to a question: the exit code, what it printed, its messages.
async fn dbr_full(st: &AppState, args: &[&str], tty: bool, answer: Answer, cancel: databrain_connector_core::CancellationToken) -> (u8, String, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let opts = parse(&args, |p| p.is_file() || p.is_dir()).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let ask = move |_: &str| answer;
    let code = run_with(st, opts, Io { out: &mut out, err: &mut err, ask: &ask, cancel }, Terminal { out: tty, width: 120 }).await;
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

/// `dbr <args>` without a terminal to ask in: the exit code and what it printed.
async fn dbr(st: &AppState, args: &[&str], tty: bool) -> (u8, String) {
    let (code, out, _) = dbr_full(st, args, tty, Answer::NoTerminal, Default::default()).await;
    (code, out)
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

    // Asked in a terminal: no leaves it, yes runs it.
    let (code, _, err) = dbr_full(&st, &["-c", "Prod", "-i", "insert into p values (2)"], false, Answer::No, Default::default()).await;
    assert_eq!(code, 2, "{err}");
    let (code, _, _) = dbr_full(&st, &["-c", "Prod", "-i", "insert into p values (2)"], false, Answer::Yes, Default::default()).await;
    assert_eq!(code, 0);
    let (_, out) = dbr(&st, &["-c", "Prod", "-i", "--csv", "select count(*) as n from p"], false).await;
    assert_eq!(out, "n\n2\n");

    // Read-only refuses the write, --yes or not, and says why.
    let (code, _, err) = dbr_full(&st, &["-c", "Archive", "-i", "--yes", "create table x (id integer)"], false, Answer::Yes, Default::default()).await;
    assert_ne!(code, 0);
    assert!(err.to_lowercase().contains("read-only"), "{err}");
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

    // A :name without a value: a bad command line (2), before anything runs.
    let (code, _, err) = dbr_full(&st, &["-c", "Sales", "-i", "select * from t where id = :id"], false, Answer::No, Default::default()).await;
    assert_eq!(code, 2);
    assert!(err.contains(":id"), "{err}");
    // Ctrl-C: stopped (130), nothing printed.
    let stop = databrain_connector_core::CancellationToken::new();
    stop.cancel();
    let (code, out, _) = dbr_full(&st, &["-c", "Sales", "-i", "--csv", many], false, Answer::No, stop).await;
    assert_eq!((code, out.as_str()), (130, ""));

    // A failed statement: exit 1.
    let (code, _) = dbr(&st, &["-c", "Sales", "-i", "select * from nope"], false).await;
    assert_eq!(code, 1);

    // In the history as the terminal's.
    let origins: Vec<Origin> = st.workspace.list_history(&Default::default()).unwrap().into_iter().map(|h| h.origin).collect();
    assert!(origins.contains(&Origin::Cli), "{origins:?}");
}

/// Data files go to the local DuckDB by their full path (the history's SQL
/// runs again from anywhere); a folder is data, whatever its name; JSON
/// keeps every column and every digit.
#[cfg(feature = "duckdb-engine")]
#[tokio::test(flavor = "multi_thread")]
async fn dbr_reads_data_files_and_folders() {
    let dir = tempfile::tempdir().unwrap();
    let st = state();
    let csv = dir.path().join("orders.csv");
    std::fs::write(&csv, "id,total\n1,120.50\n2,35.00\n").unwrap();
    let rel = pathdiff(&csv);
    let (code, out) = dbr(&st, &["-i", "--json", &rel], false).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.lines().count(), 2, "{out}");
    let sql: Vec<String> = st.workspace.list_history(&Default::default()).unwrap().into_iter().map(|h| h.sql).collect();
    assert!(
        sql.iter().any(|s| s.contains(&csv.canonicalize().unwrap().to_string_lossy().to_string())
            || s.contains(&std::path::absolute(&csv).unwrap().to_string_lossy().to_string())),
        "{sql:?}"
    );

    // A folder named without a dot (Parquet files in it) is data, not SQL.
    let folder = dir.path().join("events");
    std::fs::create_dir(&folder).unwrap();
    let target = folder.join("part-0.parquet");
    let (code, _) = dbr(&st, &["-i", &format!("COPY (SELECT 1 AS a, 2 AS b) TO '{}' (FORMAT parquet)", target.display())], false).await;
    assert_eq!(code, 0);
    let (code, out, err) = dbr_full(&st, &["-i", "--csv", &folder.to_string_lossy()], false, Answer::No, Default::default()).await;
    assert_eq!((code, out.as_str()), (0, "a,b\n1,2\n"), "{err}");

    // JSON: two columns of one name both kept; digits a double can't hold stay text.
    let (code, out) =
        dbr(&st, &["-i", "--json", "select 1 as id, 2 as id, 0.1000000000000000055::decimal(38,19) as d, 123456789012345678901234::hugeint as h"], false).await;
    assert_eq!(code, 0);
    assert_eq!(out, "{\"id\":1,\"id_2\":2,\"d\":\"0.1000000000000000055\",\"h\":\"123456789012345678901234\"}\n");
}

/// `p` relative to the current directory when it can be (as a user types it).
fn pathdiff(p: &std::path::Path) -> String {
    let cwd = std::env::current_dir().unwrap();
    let mut up = std::path::PathBuf::new();
    let mut base = cwd.as_path();
    loop {
        if let Ok(rest) = p.strip_prefix(base) {
            return up.join(rest).to_string_lossy().into_owned();
        }
        match base.parent() {
            Some(b) => {
                base = b;
                up.push("..");
            }
            None => return p.to_string_lossy().into_owned(),
        }
    }
}
