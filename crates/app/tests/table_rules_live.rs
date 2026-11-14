//! Live: where an unqualified table name resolves, asked of real servers
//! (`QueryEngine::default_namespace`), and the AI's table rules following
//! it. Each database is skipped unless its `DATABRAIN_<DB>_HOST` is set
//! (as in CI's live job; locally e.g. with podman containers).

use std::sync::Arc;

use databrain_app::api::{self, AppState, SaveConnectionArgs};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::{EventSink, JobEvent, RunRequest};
use databrain_workspace::{AiPolicy, ConnectionProfile, EnvTag, RunQueryPolicy, Workspace};

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

fn var(env: &str, name: &str) -> Option<String> {
    std::env::var(format!("{env}_{name}")).ok().filter(|v| !v.is_empty())
}

/// A connection to the server `env` names (`None`: not set), as `user`.
fn connect(
    st: &AppState,
    env: &str,
    kind: ConnectorKind,
    user: &str,
    password: Option<String>,
    database: Option<&str>,
    policy: AiPolicy,
) -> Option<ConnectionProfile> {
    let host = var(env, "HOST")?;
    let mut c = ConnectionConfig::new(kind, AuthMethod::Password { user: user.into() });
    c.host = Some(host);
    c.port = var(env, "PORT").and_then(|p| p.parse().ok());
    c.database = database.map(String::from);
    if kind == ConnectorKind::Oracle {
        c.options.insert("service_name".into(), var(env, "SERVICE").unwrap_or_else(|| "FREEPDB1".into()));
        c.options.insert("driver".into(), "thin".into());
    }
    let profile = ConnectionProfile {
        id: String::new(),
        name: format!("{kind:?} as {user}"),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: password.is_some(),
        ai_policy: policy,
        created_at: 0,
        updated_at: 0,
    };
    Some(api::save_connection(st, SaveConnectionArgs { profile, secret: password, clear_secret: false, extra_secrets: Default::default() }).unwrap())
}

async fn run(st: &AppState, conn: &str, sql: &str) {
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
    };
    let out = st.engine.run_and_wait(&st.hub, r, None).await.unwrap();
    if let Some(e) = out.iter().find_map(|o| o.error.as_ref()) {
        panic!("{}: {sql}", e.message);
    }
}

fn some(v: &str) -> Option<String> {
    Some(v.to_string())
}

/// Each server says its current database and schema, in the levels the
/// table rules use for it.
#[tokio::test(flavor = "multi_thread")]
async fn servers_say_their_current_schema() {
    let st = state();
    let admin = AiPolicy::default;
    if let Some(p) = connect(
        &st,
        "DATABRAIN_PG",
        ConnectorKind::Postgres,
        &var("DATABRAIN_PG", "USER").unwrap_or("postgres".into()),
        var("DATABRAIN_PG", "PASSWORD"),
        Some("postgres"),
        admin(),
    ) {
        assert_eq!(st.engine.default_namespace(&p.id).await.unwrap(), Some(vec![some("postgres"), some("public")]), "Postgres");
    }
    if let Some(p) = connect(
        &st,
        "DATABRAIN_MYSQL",
        ConnectorKind::Mysql,
        &var("DATABRAIN_MYSQL", "USER").unwrap_or("root".into()),
        var("DATABRAIN_MYSQL", "PASSWORD"),
        Some("mysql"),
        admin(),
    ) {
        assert_eq!(st.engine.default_namespace(&p.id).await.unwrap(), Some(vec![some("mysql")]), "MySQL");
    }
    // Oracle connects through its helper: only where one is installed (`DATABRAIN_ORACLE_AGENT`).
    if std::env::var("DATABRAIN_ORACLE_AGENT").is_ok()
        && let Some(p) = connect(
            &st,
            "DATABRAIN_ORACLE",
            ConnectorKind::Oracle,
            &var("DATABRAIN_ORACLE", "USER").unwrap_or("system".into()),
            var("DATABRAIN_ORACLE", "PASSWORD"),
            None,
            admin(),
        )
    {
        let user = p.config.auth.user().unwrap_or_default().to_ascii_uppercase();
        assert_eq!(st.engine.default_namespace(&p.id).await.unwrap(), Some(vec![Some(user)]), "Oracle");
    }
    if let Some(p) = connect(
        &st,
        "DATABRAIN_CLICKHOUSE",
        ConnectorKind::Clickhouse,
        &var("DATABRAIN_CLICKHOUSE", "USER").unwrap_or("default".into()),
        var("DATABRAIN_CLICKHOUSE", "PASSWORD"),
        None,
        admin(),
    ) {
        assert_eq!(st.engine.default_namespace(&p.id).await.unwrap(), Some(vec![some("default")]), "ClickHouse");
    }
    if let Some(p) = connect(
        &st,
        "DATABRAIN_MSSQL",
        ConnectorKind::Mssql,
        &var("DATABRAIN_MSSQL", "USER").unwrap_or("sa".into()),
        var("DATABRAIN_MSSQL", "PASSWORD"),
        None,
        admin(),
    ) {
        assert_eq!(st.engine.default_namespace(&p.id).await.unwrap(), Some(vec![some("master"), some("dbo")]), "SQL Server");
    }
}

/// The row cap the assistant's reads get (`cap_rows`) is SQL the server
/// runs: repeated and unnamed columns, a `WITH`, an order, a trailing
/// comment (a wrapping subquery, as before, failed on several of these).
#[tokio::test(flavor = "multi_thread")]
async fn capped_reads_run_on_the_server() {
    use databrain_ai::policy::cap_rows;
    let st = state();
    let servers = [
        ("DATABRAIN_PG", ConnectorKind::Postgres, "postgres", Some("postgres")),
        ("DATABRAIN_MYSQL", ConnectorKind::Mysql, "root", Some("mysql")),
        ("DATABRAIN_CLICKHOUSE", ConnectorKind::Clickhouse, "default", None),
    ];
    for (env, kind, user, db) in servers {
        let Some(p) = connect(&st, env, kind, &var(env, "USER").unwrap_or(user.into()), var(env, "PASSWORD"), db, AiPolicy::default()) else { continue };
        for sql in [
            "select 1 as id, 2 as id, count(*)",
            "with x as (select 1 as a union all select 2) select a, a from x order by 1 desc",
            "select 1 as a -- the answer",
        ] {
            // ClickHouse refuses one alias for two expressions in any query.
            if kind == ConnectorKind::Clickhouse && sql.contains("2 as id") {
                continue;
            }
            let capped = cap_rows(kind, sql, 10);
            assert!(capped.ends_with("LIMIT 10"), "{kind:?}: {capped}");
            run(&st, &p.id, &capped).await;
        }
    }
}

/// Postgres: a role whose `search_path` starts with another schema reads
/// that schema's table when it writes an unqualified name, and the table
/// rules check that table, not `public`'s.
#[tokio::test(flavor = "multi_thread")]
async fn table_rules_follow_a_roles_search_path() {
    use databrain_ai::policy::{Decision, review_sql};
    let st = state();
    let Some(admin) = connect(
        &st,
        "DATABRAIN_PG",
        ConnectorKind::Postgres,
        &var("DATABRAIN_PG", "USER").unwrap_or("postgres".into()),
        var("DATABRAIN_PG", "PASSWORD"),
        Some("postgres"),
        AiPolicy::default(),
    ) else {
        return;
    };
    for sql in [
        "drop schema if exists dbr_hr cascade",
        "drop role if exists dbr_rules_role",
        "create schema dbr_hr",
        "create table dbr_hr.dbr_pay (amount int)",
        "create table if not exists public.dbr_pay (amount int)",
        "create role dbr_rules_role login password 'dbr-rules-pw'",
        "alter role dbr_rules_role set search_path = dbr_hr, public",
        "grant usage on schema dbr_hr to dbr_rules_role",
        "grant select on dbr_hr.dbr_pay to dbr_rules_role",
    ] {
        run(&st, &admin.id, sql).await;
    }
    let rules = AiPolicy { run_query: RunQueryPolicy::AutoRead, read_deny: vec!["%.dbr_hr.%".into()], ..Default::default() };
    let role = connect(&st, "DATABRAIN_PG", ConnectorKind::Postgres, "dbr_rules_role", Some("dbr-rules-pw".into()), Some("postgres"), rules).unwrap();
    assert_eq!(st.engine.default_namespace(&role.id).await.unwrap(), Some(vec![some("postgres"), some("dbr_hr")]));
    // Before the server is asked, the usual default (`public`) is assumed: the read looks allowed.
    assert_eq!(review_sql(&role, "select * from dbr_pay").decision, Decision::Allow);
    databrain_ai::access::learn_defaults(&st.engine, &role).await;
    match review_sql(&role, "select * from dbr_pay").decision {
        Decision::Deny(r) => assert!(r.contains("postgres.dbr_hr.dbr_pay"), "{r}"),
        d => panic!("the role reads dbr_hr.dbr_pay, which the rules deny: {d:?}"),
    }
    // A name that says its schema is what it says.
    assert_eq!(review_sql(&role, "select * from public.dbr_pay").decision, Decision::Allow);
    st.engine.disconnect(&role.id);
    for sql in ["drop schema dbr_hr cascade", "drop table public.dbr_pay", "drop role dbr_rules_role"] {
        run(&st, &admin.id, sql).await;
    }
}

/// DataBrain's own catalog queries keep a table's name inside its string
/// literal, whatever the name holds. MySQL reads `\` as an escape, so a
/// name with `\'` used to end the literal early and run the rest as SQL
/// (statements can be stacked there): the canary table must still exist,
/// and the layout must be that of the oddly named table.
#[tokio::test(flavor = "multi_thread")]
async fn hostile_table_names_stay_inside_the_literal() {
    let st = state();
    if let Some(p) = connect(
        &st,
        "DATABRAIN_MYSQL",
        ConnectorKind::Mysql,
        &var("DATABRAIN_MYSQL", "USER").unwrap_or("root".into()),
        var("DATABRAIN_MYSQL", "PASSWORD"),
        Some("mysql"),
        AiPolicy::default(),
    ) {
        let name = r"x\';DROP TABLE dbr_names.canary;#";
        for sql in [
            "drop database if exists dbr_names".to_string(),
            "create database dbr_names".to_string(),
            "create table dbr_names.canary (a int)".to_string(),
            format!("create table dbr_names.`{name}` (id int primary key, v int, key by_v (v))"),
        ] {
            run(&st, &p.id, &sql).await;
        }
        let layout = st.engine.table_layout(&p.id, "dbr_names", name).await.unwrap();
        let mut indexes: Vec<&str> = layout.indexes.iter().map(|i| i.name.as_str()).collect();
        indexes.sort_unstable();
        assert_eq!(indexes, ["PRIMARY", "by_v"], "the table of that name was read");
        // Still there: nothing after the name ran.
        run(&st, &p.id, "select count(*) from dbr_names.canary").await;
        run(&st, &p.id, "drop database dbr_names").await;
    }
    if let Some(p) = connect(
        &st,
        "DATABRAIN_PG",
        ConnectorKind::Postgres,
        &var("DATABRAIN_PG", "USER").unwrap_or("postgres".into()),
        var("DATABRAIN_PG", "PASSWORD"),
        Some("postgres"),
        AiPolicy::default(),
    ) {
        let name = r"x\' union select 'a','b',true,true,'c' --";
        for sql in [
            "drop schema if exists dbr_names cascade".to_string(),
            "create schema dbr_names".to_string(),
            format!("create table dbr_names.\"{name}\" (id int primary key, v int)"),
            format!("create index by_v on dbr_names.\"{name}\" (v)"),
        ] {
            run(&st, &p.id, &sql).await;
        }
        // Also with backslashes read as escapes on this session's role (the old default).
        for scs in ["on", "off"] {
            run(&st, &p.id, &format!("alter role current_user set standard_conforming_strings = {scs}")).await;
            st.engine.disconnect(&p.id);
            let layout = st.engine.table_layout(&p.id, "dbr_names", name).await.unwrap();
            assert_eq!(layout.indexes.len(), 2, "standard_conforming_strings = {scs}: {:?}", layout.indexes.iter().map(|i| &i.name).collect::<Vec<_>>());
            assert!(layout.indexes.iter().any(|i| i.name == "by_v"));
        }
        run(&st, &p.id, "alter role current_user reset standard_conforming_strings").await;
        run(&st, &p.id, "drop schema dbr_names cascade").await;
    }
}
