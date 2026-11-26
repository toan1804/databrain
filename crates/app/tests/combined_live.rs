//! Live: pushdown of combined queries on real servers (PostgreSQL, MySQL,
//! SQL Server): the same rows with it on and off, and what was pushed in
//! each dialect. `#[ignore]`d: run with `cargo test -- --ignored` (CI's live
//! job; locally e.g. with podman containers). Each database is skipped
//! unless its `DATABRAIN_<DB>_HOST` is set.

use std::sync::Arc;

use databrain_app::api::{self, AppState, SaveConnectionArgs};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::{EventSink, JobEvent, RunRequest, StatementOutcomeView};
use databrain_workspace::{AiPolicy, ConnectionProfile, EnvTag, Workspace};

/// Keeps every event.
#[derive(Default)]
struct Events(parking_lot::Mutex<Vec<JobEvent>>);
impl EventSink for Events {
    fn emit(&self, e: JobEvent) {
        self.0.lock().push(e);
    }
}

struct NoUi;
impl databrain_app::ai_api::UiBridge for NoUi {
    fn emit(&self, _: &str, _: serde_json::Value) {}
    fn open_url(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

fn var(env: &str, name: &str) -> Option<String> {
    std::env::var(format!("{env}_{name}")).ok().filter(|v| !v.is_empty())
}

/// A connection named `name` to the server `env` names (`None`: not set).
fn connect(st: &AppState, env: &str, kind: ConnectorKind, name: &str, user: &str, database: Option<&str>) -> Option<ConnectionProfile> {
    let host = var(env, "HOST")?;
    let mut c = ConnectionConfig::new(kind, AuthMethod::Password { user: user.into() });
    c.host = Some(host);
    c.port = var(env, "PORT").and_then(|p| p.parse().ok());
    c.database = database.map(String::from);
    let password = var(env, "PASSWORD");
    let profile = ConnectionProfile {
        id: String::new(),
        name: name.into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: password.is_some(),
        ai_policy: AiPolicy::default(),
        created_at: 0,
        updated_at: 0,
    };
    Some(api::save_connection(st, SaveConnectionArgs { profile, secret: password, clear_secret: false, extra_secrets: Default::default() }).unwrap())
}

async fn run(st: &AppState, conn: &str, sql: &str, pushdown: Option<bool>) -> Vec<StatementOutcomeView> {
    let r = RunRequest {
        connection_id: conn.into(),
        tab_id: format!("t-{}", uuid::Uuid::new_v4()),
        sql: sql.into(),
        base_offset: 0,
        row_limit: None,
        confirmed: true,
        origin: Default::default(),
        session_key: None,
        output_name: None,
        params: Default::default(),
        output_masks: None,
        part_rows: None,
        pushdown,
    };
    let out = st.engine.run_and_wait(&st.hub, r, None).await.unwrap();
    if let Some(e) = out.iter().find_map(|o| o.error.as_ref()) {
        panic!("{}: {sql}", e.message);
    }
    out
}

/// The last statement's rows as text, `|` between values, sorted.
fn rows(st: &AppState, out: &[StatementOutcomeView]) -> Vec<String> {
    let r = out.last().and_then(|o| o.result.as_ref()).expect("a result");
    let rs = st.engine.results().get(&r.id).unwrap();
    let mut g = rs.lock();
    let page = g.page(&databrain_result_store::ViewSpec::default(), 0, 10_000).unwrap();
    let mut v: Vec<String> = page.rows.iter().map(|row| row.iter().map(|x| x.clone().unwrap_or_default()).collect::<Vec<_>>().join("|")).collect();
    v.sort();
    v
}

/// Each server's table: an integer, an exact decimal, characters, a date and
/// a timestamp without a time zone, in its own types.
fn setup_sql(kind: ConnectorKind) -> Vec<&'static str> {
    match kind {
        ConnectorKind::Postgres => {
            vec!["drop table if exists dbr_pd", "create table dbr_pd (id integer, amount numeric(10,2), name varchar(20), day date, ts timestamp)"]
        }
        ConnectorKind::Mysql => {
            vec!["drop table if exists dbr_pd", "create table dbr_pd (id integer, amount decimal(10,2), name varchar(20), day date, ts datetime(6))"]
        }
        ConnectorKind::Mssql => vec![
            "if object_id('dbo.dbr_pd') is not null drop table dbo.dbr_pd",
            "create table dbo.dbr_pd (id int, amount decimal(10,2), name nvarchar(20), day date, ts datetime2)",
        ],
        _ => unreachable!(),
    }
}

const ROWS: &str = "insert into dbr_pd values (1, 5.50, 'a', '2024-01-01', '2024-01-01 09:00:00'), \
                    (2, 12.25, 'b', '2024-01-02', '2024-01-01 10:00:00'), (3, 40.00, 'Hà Nội', '2024-01-03', '2024-01-02 08:30:00'), \
                    (4, null, null, null, null)";

#[ignore = "live: needs DATABRAIN_PG_HOST, DATABRAIN_MYSQL_HOST or DATABRAIN_MSSQL_HOST (cargo test -- --ignored)"]
#[tokio::test(flavor = "multi_thread")]
async fn pushdown_gives_the_same_rows_on_real_servers() {
    let events = Arc::new(Events::default());
    let st = AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), events.clone(), Arc::new(NoUi));
    let mut servers: Vec<(ConnectorKind, ConnectionProfile)> = Vec::new();
    if let Some(p) = connect(&st, "DATABRAIN_PG", ConnectorKind::Postgres, "pd_pg", &var("DATABRAIN_PG", "USER").unwrap_or("postgres".into()), Some("postgres"))
    {
        servers.push((ConnectorKind::Postgres, p));
    }
    if let Some(admin) =
        connect(&st, "DATABRAIN_MYSQL", ConnectorKind::Mysql, "pd_my_admin", &var("DATABRAIN_MYSQL", "USER").unwrap_or("root".into()), Some("mysql"))
    {
        run(&st, &admin.id, "create database if not exists dbr_pd_db", None).await;
        let p = connect(&st, "DATABRAIN_MYSQL", ConnectorKind::Mysql, "pd_my", &var("DATABRAIN_MYSQL", "USER").unwrap_or("root".into()), Some("dbr_pd_db"))
            .unwrap();
        servers.push((ConnectorKind::Mysql, p));
    }
    if let Some(p) = connect(&st, "DATABRAIN_MSSQL", ConnectorKind::Mssql, "pd_ms", &var("DATABRAIN_MSSQL", "USER").unwrap_or("sa".into()), None) {
        servers.push((ConnectorKind::Mssql, p));
    }
    assert!(!servers.is_empty(), "live test: no DATABRAIN_<DB>_HOST set (run with --ignored)");
    for (kind, p) in &servers {
        for sql in setup_sql(*kind) {
            run(&st, &p.id, sql, None).await;
        }
        run(&st, &p.id, ROWS, None).await;
        let part = format!("@{} select * from dbr_pd", p.name);
        let queries = [
            format!("with t as ({part}) select id, amount from t where amount > 10"),
            format!("with t as ({part}) select id from t where amount between 5 and 13"),
            format!("with t as ({part}) select id from t where name = 'b' or name = 'Hà Nội'"),
            format!("with t as ({part}) select id from t where name in ('a', 'b')"),
            format!("with t as ({part}) select id from t where day >= '2024-01-02'"),
            format!("with t as ({part}) select id from t where ts > '2024-01-01 09:30:00' and ts < '2024-01-02 00:00:00'"),
            format!("with t as ({part}) select id from t where id <> 2 and amount is not null"),
            format!("with t as ({part}) select id, name from t where id in (1, 3)"),
            format!("with t as ({part}) select count(*) from t where name is null"),
            // Every operator on timestamps where they hold microseconds (PostgreSQL, MySQL's DATETIME).
            format!("with t as ({part}) select id from t where ts = '2024-01-01 10:00:00'"),
            format!("with t as ({part}) select id from t where ts between '2024-01-01 09:00:00' and '2024-01-01 10:00:00'"),
            format!("with t as ({part}) select id from t where ts in ('2024-01-01 09:00:00', '2024-01-02 08:30:00')"),
        ];
        for q in &queries {
            let on = rows(&st, &run(&st, &p.id, q, Some(true)).await);
            let off = rows(&st, &run(&st, &p.id, q, Some(false)).await);
            assert_eq!(on, off, "{kind:?}: {q}");
        }
        // What went to the server: a wrapper in its dialect with the decimal,
        // text and timestamp terms.
        let sent: Vec<String> = events
            .0
            .lock()
            .iter()
            .filter_map(|e| match e {
                JobEvent::PartFinished { connection_name, output, .. } if connection_name == &p.name => Some(output.sql.clone()),
                _ => None,
            })
            .collect();
        let ts_eq = if *kind == ConnectorKind::Mssql { None } else { Some(" = TIMESTAMP '2024-01-01 10:00:00'") };
        if let Some(t) = ts_eq {
            assert!(
                sent.iter().any(|s| s.contains("BETWEEN") && s.contains("ts")) || sent.iter().any(|s| s.contains(t)),
                "{kind:?}: no timestamp `=`/BETWEEN pushed: {sent:?}"
            );
        }
        for needle in ["databrain_part", "> 10", "BETWEEN 5 AND 13", "IS NOT NULL"] {
            assert!(sent.iter().any(|s| s.contains(needle)), "{kind:?}: nothing sent holds {needle}: {sent:?}");
        }
        let text = if *kind == ConnectorKind::Mssql { "= N'b'" } else { "= 'b'" };
        assert!(sent.iter().any(|s| s.contains(text)), "{kind:?}: {sent:?}");
        // A limit on a plain one-part query.
        let out = run(&st, &p.id, &format!("with t as ({part}) select id from t limit 2"), Some(true)).await;
        assert_eq!(rows(&st, &out).len(), 2, "{kind:?}");
        // Values a server holds but that arrive as NULL (PostgreSQL's
        // infinity), MySQL's TIMESTAMP (time zone) and YEAR (a constant is
        // a year): the same rows with pushdown on and off.
        let edge: Vec<(&str, Vec<String>)> = match kind {
            ConnectorKind::Postgres => vec![
                ("insert into dbr_pd (id, day, ts) values (5, 'infinity', '-infinity')", vec![]),
                (
                    "",
                    vec![
                        format!("with t as ({part}) select count(*) from t where day is null"),
                        format!("with t as ({part}) select count(*) from t where ts is null"),
                        format!("with t as ({part}) select id from t where day > '2024-01-01'"),
                    ],
                ),
            ],
            ConnectorKind::Mysql => vec![
                ("alter table dbr_pd add column at_utc timestamp null, add column y year null", vec![]),
                ("update dbr_pd set at_utc = '2024-03-10 02:30:00', y = 2050 where id = 1", vec![]),
                (
                    "",
                    vec![
                        format!("with t as ({part}) select id from t where y > 50"),
                        format!("with t as ({part}) select id from t where at_utc > '2024-03-10 01:00:00'"),
                        format!("with t as ({part}) select count(*) from t where at_utc is null"),
                    ],
                ),
            ],
            _ => vec![],
        };
        for (setup, qs) in &edge {
            if !setup.is_empty() {
                run(&st, &p.id, setup, None).await;
            }
            for q in qs {
                let on = rows(&st, &run(&st, &p.id, q, Some(true)).await);
                let off = rows(&st, &run(&st, &p.id, q, Some(false)).await);
                assert_eq!(on, off, "{kind:?}: {q}");
            }
        }
        // MySQL's TIMESTAMP and YEAR are never pushed (on a UTC server the
        // rows would match anyway; what was sent says so).
        if *kind == ConnectorKind::Mysql {
            let all: Vec<String> = events
                .0
                .lock()
                .iter()
                .filter_map(|e| match e {
                    JobEvent::PartFinished { output, .. } => Some(output.sql.clone()),
                    _ => None,
                })
                .collect();
            assert!(all.iter().all(|s| !s.contains("`at_utc` >") && !s.contains("`y` >")), "{all:?}");
        }
        let drop = if *kind == ConnectorKind::Mssql { "drop table dbo.dbr_pd" } else { "drop table dbr_pd" };
        run(&st, &p.id, drop, None).await;
    }
}
