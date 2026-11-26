//! Combined queries: parts on other connections joined in DuckDB
//! (docs/design/cross-connection-queries.md, phase 1).

use std::collections::HashMap;
use std::sync::Arc;

use databrain_app::api::AppState;
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::sql::ParamValue;
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::{EngineError, EventSink, JobEvent, RunRequest, StatementOutcomeView};
use databrain_workspace::{ConnectionProfile, EnvTag, Workspace};

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

struct Setup {
    st: AppState,
    events: Arc<Events>,
    sales: String,
    crm: String,
    _dir: tempfile::TempDir,
}

fn sqlite(ws: &Workspace, dir: &std::path::Path, name: &str, read_only: bool) -> String {
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(dir.join(format!("{name}.db")).to_string_lossy().into());
    c.read_only = read_only;
    ws.save_connection(ConnectionProfile {
        id: String::new(),
        name: name.into(),
        config: c,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    })
    .unwrap()
    .id
}

fn req(conn: &str, tab: &str, sql: &str) -> RunRequest {
    RunRequest {
        connection_id: conn.into(),
        tab_id: tab.into(),
        sql: sql.into(),
        base_offset: 0,
        row_limit: Some(1000),
        confirmed: true,
        origin: Default::default(),
        session_key: None,
        output_name: None,
        params: Default::default(),
        output_masks: None,
        part_rows: None,
        pushdown: None,
    }
}

async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(Workspace::open_in_memory().unwrap());
    let sales = sqlite(&ws, dir.path(), "Sales DB", false);
    let crm = sqlite(&ws, dir.path(), "crm", true);
    let events = Arc::new(Events::default());
    let st = AppState::new(ws.clone(), Arc::new(MemoryStore::default()), events.clone(), Arc::new(NoUi));
    let s = Setup { st, events, sales, crm, _dir: dir };
    s.ok(&s.sales, "create table orders (id integer, customer_id integer, total real); insert into orders values (1, 10, 5.5), (2, 10, 4.5), (3, 20, 7)").await;
    // crm is read-only: its table is made before it is marked so.
    let mut p = s.st.workspace.get_connection(&s.crm).unwrap();
    p.config.read_only = false;
    s.st.workspace.save_connection(p.clone()).unwrap();
    s.ok(&s.crm, "create table customers (id integer, name text); insert into customers values (10, 'Ann'), (20, 'Bo'), (30, 'Cy')").await;
    p.config.read_only = true;
    s.st.workspace.save_connection(p).unwrap();
    s.st.engine.disconnect(&s.crm);
    s
}

impl Setup {
    async fn run(&self, r: RunRequest) -> Result<Vec<StatementOutcomeView>, EngineError> {
        self.st.engine.run_and_wait(&self.st.hub, r, None).await
    }

    async fn ok(&self, conn: &str, sql: &str) -> Vec<StatementOutcomeView> {
        let out = self.run(req(conn, "setup", sql)).await.unwrap();
        assert!(out.iter().all(|o| o.error.is_none()), "{:?}", out.iter().map(|o| &o.error).collect::<Vec<_>>());
        out
    }

    /// Rows of a result as text, `|` between values.
    fn rows(&self, o: &StatementOutcomeView) -> Vec<String> {
        let r = o.result.as_ref().expect("a result");
        let rs = self.st.engine.results().get(&r.id).unwrap();
        let mut g = rs.lock();
        let page = g.page(&databrain_result_store::ViewSpec::default(), 0, 1000).unwrap();
        page.rows.iter().map(|row| row.iter().map(|v| v.clone().unwrap_or_default()).collect::<Vec<_>>().join("|")).collect()
    }
}

const JOIN: &str = "with o as (@\"Sales DB\" select id, customer_id, total from orders),\n     c(cid, name) as (@crm select id, name from customers)\nselect c.name, sum(o.total) as total from o join c on c.cid = o.customer_id group by 1 order by 1";

#[tokio::test(flavor = "multi_thread")]
async fn parts_on_two_connections_are_joined() {
    let s = setup().await;
    // From a tab on either connection, or on neither (the Results one).
    for tab_conn in [s.sales.clone(), s.crm.clone(), s.st.engine.results_connection().unwrap().id] {
        let out = s.run(req(&tab_conn, "t1", JOIN)).await.unwrap();
        assert!(out[0].error.is_none(), "{:?}", out[0].error);
        assert_eq!(s.rows(&out[0]), vec!["Ann|10.0", "Bo|7.0"]);
    }
    // Each part's rows are a result of the tab, from its connection.
    let parts: Vec<(String, String, usize)> = s
        .events
        .0
        .lock()
        .iter()
        .filter_map(|e| match e {
            JobEvent::PartFinished { part, connection_name, output, .. } => Some((part.clone(), connection_name.clone(), output.rows)),
            _ => None,
        })
        .collect();
    assert!(parts.contains(&("o".into(), "Sales DB".into(), 3)) && parts.contains(&("c".into(), "crm".into(), 3)), "{parts:?}");
    let outputs = s.st.engine.outputs().list();
    let of_tab: Vec<_> = outputs.iter().filter(|o| o.tab_id == "t1" && o.active).collect();
    assert_eq!(of_tab.len(), 3, "the join and its two parts");
    assert!(of_tab.iter().any(|o| o.connection_id == s.crm && o.sql == "select id, name from customers"));
    // Each part is in its connection's history; the join in Results'.
    let hist = s.st.workspace.list_history(&Default::default()).unwrap();
    assert!(hist.iter().any(|h| h.connection_id.as_deref() == Some(s.crm.as_str()) && h.sql == "select id, name from customers"));
}

#[tokio::test(flavor = "multi_thread")]
async fn parameters_and_scripts() {
    let s = setup().await;
    let mut r = req(
        &s.sales,
        "t2",
        "select count(*) from orders;\nwith o as (@\"Sales DB\" select * from orders where total > :min) select count(*) as n from o where total > :min",
    );
    r.params = HashMap::from([("min".to_string(), ParamValue { value: "5".into(), raw: false })]);
    let out = s.run(r).await.unwrap();
    assert_eq!(s.rows(&out[0]), vec!["3"]);
    assert_eq!(s.rows(&out[1]), vec!["2"]);
    // A missing value is asked for, as for any statement.
    let e = s.run(req(&s.sales, "t2", "with o as (@\"Sales DB\" select * from orders where id = :id) select * from o")).await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("missing_parameters"), "{e:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_reads_run_and_nothing_runs_when_one_part_is_refused() {
    let s = setup().await;
    let refused = |sql: &str| {
        let s = &s;
        let sql = sql.to_string();
        async move { s.run(req(&s.sales, "t3", &sql)).await.unwrap_err() }
    };
    // A part that writes, even on a writable connection.
    let e = refused("with a as (@\"Sales DB\" delete from orders returning id), b as (@crm select 1) select * from a").await;
    assert!(e.message.contains("Part `a` (on \"Sales DB\") must be a query that only reads"), "{}", e.message);
    // The outer query, in DuckDB.
    let e = refused("with a as (@crm select 1 as x) insert into t select * from a").await;
    assert!(e.message.contains("The query that reads the parts must only read"), "{}", e.message);
    // An unknown connection: the close names, never another connection.
    let e = refused("with a as (@cmr select 1) select * from a").await;
    assert!(e.message.contains("Part `a`: no connection named \"cmr\"; did you mean \"crm\"?"), "{}", e.message);
    assert_eq!(e.position, Some(12));
    // Nothing ran: the orders are all there.
    let out = s.ok(&s.sales, "select count(*) from orders").await;
    assert_eq!(s.rows(&out[0]), vec!["3"]);
    // Not from the assistant yet.
    let mut r = req(&s.sales, "t3", JOIN);
    r.origin = databrain_workspace::Origin::Ai;
    assert_eq!(s.run(r).await.unwrap_err().kind, "unsupported");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_part_past_the_row_limit_fails_instead_of_being_cut() {
    let s = setup().await;
    let mut r = req(&s.sales, "t4", JOIN);
    r.part_rows = Some(2);
    let out = s.run(r).await.unwrap();
    let e = out[0].error.as_ref().expect("refused");
    assert_eq!(e.code.as_deref(), Some(databrain_query_engine::PART_TOO_LARGE));
    assert!(e.message.contains("has more than 2 rows, and a part is read whole"), "{}", e.message);
    assert!(out[0].result.is_none());
    // "Run again without the limit".
    let mut r = req(&s.sales, "t4", JOIN);
    r.part_rows = Some(0);
    let out = s.run(r).await.unwrap();
    assert_eq!(s.rows(&out[0]), vec!["Ann|10.0", "Bo|7.0"]);
    // The tab's row limit is the join's only.
    let mut r = req(&s.sales, "t4", "with o as (@\"Sales DB\" select * from orders) select count(*) from o");
    r.row_limit = Some(1);
    assert_eq!(s.rows(&s.run(r).await.unwrap()[0]), vec!["3"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_part_names_itself_and_where_it_is() {
    let s = setup().await;
    let sql = "with a as (@crm select nope from customers) select * from a";
    let out = s.run(req(&s.sales, "t5", sql)).await.unwrap();
    let e = out[0].error.as_ref().expect("failed");
    assert!(e.message.starts_with("Part `a` (on \"crm\"): "), "{}", e.message);
    // In the statement, at or after the part's SQL.
    assert!(e.position.is_some_and(|p| p as usize > sql.find("select nope").unwrap()), "{e:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_part_on_the_tabs_connection_waits_for_its_transaction() {
    let s = setup().await;
    s.run(req(&s.sales, "t6", "begin")).await.unwrap();
    s.run(req(&s.sales, "t6", "insert into orders values (4, 30, 1)")).await.unwrap();
    let e = s.run(req(&s.sales, "t6", JOIN)).await.unwrap_err();
    assert!(e.message.contains("wouldn't see this tab's open transaction"), "{}", e.message);
    // Another tab, or after the commit: it runs.
    assert!(s.run(req(&s.sales, "t7", JOIN)).await.unwrap()[0].error.is_none());
    s.run(req(&s.sales, "t6", "commit")).await.unwrap();
    let out = s.run(req(&s.sales, "t6", JOIN)).await.unwrap();
    assert_eq!(s.rows(&out[0]), vec!["Ann|10.0", "Bo|7.0", "Cy|1.0"]);
}

/// The join's output is the SQL that ran, reading each part as
/// `results.<handle>`: what reads the join later (the assistant's policy and
/// masks, which follow `results.<name>` to the parts' connections; an export
/// of every row) traces it to the parts.
#[tokio::test(flavor = "multi_thread")]
async fn the_joins_output_reads_the_parts_outputs() {
    let s = setup().await;
    let out = s.run(req(&s.sales, "t8", JOIN)).await.unwrap();
    let join = s.st.engine.outputs().list().into_iter().find(|o| o.result_id == out[0].result.as_ref().unwrap().id).unwrap();
    assert!(!join.sql.contains('@') && join.sql.contains("results.r"), "{}", join.sql);
    let inputs = s.st.engine.outputs().referenced(&join.sql);
    let mut conns: Vec<&str> = inputs.iter().map(|o| o.connection_name.as_str()).collect();
    conns.sort();
    assert_eq!(conns, ["Sales DB", "crm"]);
    // Every row again, from the outputs (a cut join can be exported whole).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.csv");
    let n = databrain_app::api::export_full_result(
        &s.st,
        &out[0].result.as_ref().unwrap().id,
        databrain_export::ExportOptions::new(databrain_export::ExportFormat::Csv),
        path.to_string_lossy().into_owned(),
        "x1",
    )
    .await
    .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn who_and_where_may_run_one() {
    let s = setup().await;
    // Not the assistant, MCP or a job: refused before any connection is
    // looked up, so the refusal names none.
    for origin in [databrain_workspace::Origin::Ai, databrain_workspace::Origin::Mcp, databrain_workspace::Origin::Job] {
        // A part that doesn't parse (an unclosed parenthesis) answers the
        // same, whether its name is a connection or not.
        for sql in [
            "with a as (@cmr select 1) select * from a",
            "with a as (@cmr select (1) select * from a",
            "with a as (@crm select (1) select * from a",
            "with a as (@nothing_like_it select (1) select * from a",
        ] {
            let mut r = req(&s.sales, "t9", sql);
            r.origin = origin;
            let e = s.run(r).await.unwrap_err();
            assert_eq!(e.kind, "unsupported", "{origin:?} {sql}");
            assert!(!e.message.contains("crm") && !e.message.contains("Sales") && !e.message.contains("parenthesis"), "{}", e.message);
        }
    }
    // Two parts of one name.
    let e = s.run(req(&s.sales, "t9", "with a as (@crm select 1), A as (@crm select 2) select * from a")).await.unwrap_err();
    assert!(e.message.contains("Two parts are named `A`"), "{}", e.message);
    // Not in a notebook or a dashboard yet.
    for key in ["nb:n1:c1", "dash:d1:c1"] {
        let e = s.run(req(&s.sales, key, JOIN)).await.unwrap_err();
        assert_eq!(e.kind, "unsupported", "{key}");
    }
    // The assistant's ordinary WITH query with `@` in it is no part of this.
    let mut r = req(&s.st.engine.results_connection().unwrap().id, "t9", "with a as (select 1 as x) select x from a where '@pg' <> ''");
    r.origin = databrain_workspace::Origin::Ai;
    assert!(s.run(r).await.unwrap()[0].error.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_outer_query_and_every_part_only_read() {
    let s = setup().await;
    for (sql, why) in [
        ("with a as (@crm select 1 as x) select * from a; copy (select 1) to '/tmp/x.csv'", "statement"),
        ("with a as (@crm select 1 as x) copy (select * from a) to '/tmp/x.csv'", "must only read"),
        ("with a as (@\"Sales DB\" vacuum) select * from a", "must be a query that only reads"),
        ("with a as (@crm select 1; delete from customers) select * from a", "must be a query that only reads"),
    ] {
        match s.run(req(&s.sales, "t10", sql)).await {
            Err(e) => assert!(e.message.contains(why) || e.kind == "invalid", "{sql}: {e:?}"),
            Ok(out) => assert!(out.iter().any(|o| o.error.is_some()), "{sql} ran"),
        }
    }
    let out = s.ok(&s.crm, "select count(*) from customers").await;
    assert_eq!(s.rows(&out[0]), vec!["3"]);
    // A production connection's reads need no confirmation, as in a tab.
    let mut p = s.st.workspace.get_connection(&s.sales).unwrap();
    p.env = EnvTag::Prod;
    s.st.workspace.save_connection(p).unwrap();
    let mut r = req(&s.crm, "t10", JOIN);
    r.confirmed = false;
    assert_eq!(s.rows(&s.run(r).await.unwrap()[0]), vec!["Ann|10.0", "Bo|7.0"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn many_parts_and_a_transaction_opened_in_the_same_script() {
    let s = setup().await;
    // More parts than run at once.
    let parts: Vec<String> = (0..6).map(|i| format!("p{i} as (@crm select {i} as n)")).collect();
    let union = (0..6).map(|i| format!("select n from p{i}")).collect::<Vec<_>>().join(" union all ");
    let sql = format!("with {} select sum(n) from ({union})", parts.join(", "));
    assert_eq!(s.rows(&s.run(req(&s.sales, "t11", &sql)).await.unwrap()[0]), vec!["15"]);
    // BEGIN in the same run: the part is refused when it comes to run.
    let out = s.run(req(&s.sales, "t12", &format!("begin; {JOIN}"))).await.unwrap();
    let e = out[1].error.as_ref().expect("refused");
    assert!(e.message.contains("open transaction"), "{e:?}");
    s.run(req(&s.sales, "t12", "rollback")).await.unwrap();
}

/// Pushdown (phase 2): a filter of the outer query is also sent to the part's
/// server, which then sends fewer rows; the answer is the same either way.
#[tokio::test(flavor = "multi_thread")]
async fn pushdown_asks_for_less_and_changes_no_answer() {
    let s = setup().await;
    let q = "with o as (@\"Sales DB\" select * from orders) select id, total from o where id > 1 and total > 5 order by id";
    let on = s.run(req(&s.sales, "p1", q)).await.unwrap();
    assert_eq!(s.rows(&on[0]), vec!["3|7.0"]);
    let part = s
        .events
        .0
        .lock()
        .iter()
        .rev()
        .find_map(|e| match e {
            JobEvent::PartFinished { tab_id, pushed, output, .. } if tab_id == "p1" => Some((pushed.clone(), output.sql.clone(), output.rows)),
            _ => None,
        })
        .unwrap();
    // The integer filter goes to the server; the float one (SQLite REAL) stays in DuckDB.
    assert!(part.0.iter().any(|w| w == "\"id\" > 1") && !part.0.iter().any(|w| w.contains("total >")), "{:?}", part.0);
    assert!(part.1.contains("databrain_part") && part.1.contains("WHERE \"id\" > 1"), "{}", part.1);
    assert_eq!(part.2, 2, "the server sent only the rows that pass");
    // The same rows with it off, the part as written.
    let queries = [
        q,
        JOIN,
        "with o as (@\"Sales DB\" select * from orders), c as (@crm select * from customers) select c.name, o.id from c left join o on o.customer_id = c.id where o.id is null order by 1",
        "with o as (@\"Sales DB\" select * from orders), c as (@crm select * from customers) select c.name, o.total from o join c on c.id = o.customer_id and c.name in ('Ann', 'Cy') where o.total between 4 and 6 order by 2",
        "with c as (@crm select * from customers) select name from c where name = 'Bo' or id = 30 order by 1",
        "with o as (@\"Sales DB\" select * from orders) select id from o where id <> 2 limit 1",
    ];
    for q in queries {
        let mut off = req(&s.sales, "p2", q);
        off.pushdown = Some(false);
        let a = s.rows(&s.run(req(&s.sales, "p1", q)).await.unwrap()[0]);
        let b = s.rows(&s.run(off).await.unwrap()[0]);
        assert_eq!(a, b, "{q}");
    }
    // The left join's IS NULL on the nullable side: Cy has no order.
    let mut off = req(&s.sales, "p2", queries[2]);
    off.pushdown = Some(false);
    assert_eq!(s.rows(&s.run(off).await.unwrap()[0]), vec!["Cy|"]);
}

/// Part outputs say they are parts: a kept join refreshed stays kept (a part
/// never takes its place), and the parts aren't statements of their own.
#[tokio::test(flavor = "multi_thread")]
async fn parts_are_marked_as_parts() {
    let s = setup().await;
    let out = s.run(req(&s.sales, "k1", JOIN)).await.unwrap();
    let rid = out[0].result.as_ref().unwrap().id.clone();
    let of_tab = || s.st.engine.outputs().list().into_iter().filter(|o| o.tab_id == "k1" && o.active).collect::<Vec<_>>();
    let join = of_tab().into_iter().find(|o| o.result_id == rid).unwrap();
    assert!(join.part.is_none());
    let mut names: Vec<String> = of_tab().into_iter().filter_map(|o| o.part).collect();
    names.sort();
    assert_eq!(names, ["c", "o"]);
    s.st.engine.outputs().set_kept(&join.handle, true).unwrap();
    let started = s.st.engine.rerun_output(req(&s.sales, "k1", JOIN), &join.handle).unwrap();
    let databrain_query_engine::RunResponse::Started { job_id, .. } = started else { panic!("not started") };
    while s.st.engine.is_running(&job_id) {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let now = of_tab();
    let new_join = now.iter().find(|o| o.part.is_none() && o.handle != join.handle).expect("a new join result");
    assert!(new_join.kept, "the refreshed join is kept");
    assert!(now.iter().filter(|o| o.part.is_some()).all(|o| !o.kept), "no part took the keep");
}

/// Errors point where they are in the editor: a refusal before the run
/// names its statement (bytes here; the app gives the UI UTF-16), and a
/// server's position counts in the text as written, not as bound.
#[tokio::test(flavor = "multi_thread")]
async fn errors_point_where_they_are_written() {
    let s = setup().await;
    // A part that writes, in the second statement of a script.
    let script = "select 1;\nwith a as (@\"Sales DB\" delete from orders returning id) select * from a";
    let e = s.run(req(&s.sales, "t9", script)).await.unwrap_err();
    let at = e.statement.expect("its statement");
    assert_eq!((at.index, &script[at.start..at.end]), (1, &script[10..]));
    assert_eq!(e.position, Some(script[10..].find("delete").unwrap() as u32 + 1));
    // A read-only connection's refusal names its statement too.
    let e = s.run(req(&s.crm, "t9", "select 1;\ndelete from customers")).await.unwrap_err();
    assert_eq!(e.statement.map(|a| a.index), Some(1), "{e:?}");
    // A value longer than its `:name` before the error: the position is the written one.
    let long = HashMap::from([("v".to_string(), ParamValue { value: "a much longer value than the name".into(), raw: false })]);
    let plain = "select :v, nope from orders";
    let mut r = req(&s.sales, "t9", plain);
    r.params = long.clone();
    let out = s.run(r).await.unwrap();
    assert_eq!(out[0].error.as_ref().and_then(|e| e.position), Some(plain.find("nope").unwrap() as u32 + 1), "{:?}", out[0].error);
    // The same in a part (it runs as bound: nothing to push).
    let part = "with a as (@crm select :v, nope from customers) select * from a";
    let mut r = req(&s.sales, "t9", part);
    r.params = long;
    let out = s.run(r).await.unwrap();
    assert_eq!(out[0].error.as_ref().and_then(|e| e.position), Some(part.find("nope").unwrap() as u32 + 1), "{:?}", out[0].error);
}

/// Two combined statements of one run, each with a part of the same name:
/// each part keeps its own result (their ids carry the statement and the
/// part's place, not its name), and each join reads its own part.
#[tokio::test(flavor = "multi_thread")]
async fn parts_of_one_name_in_two_statements_keep_their_own_rows() {
    let s = setup().await;
    let script = "with o as (@\"Sales DB\" select id from orders where id = 1) select id from o;\n\
                  with o as (@\"Sales DB\" select id from orders where id = 3) select id from o";
    let out = s.run(req(&s.sales, "k2", script)).await.unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(s.rows(&out[0]), ["1"]);
    assert_eq!(s.rows(&out[1]), ["3"]);
    let parts: Vec<(String, String)> =
        s.st.engine.outputs().list().into_iter().filter(|o| o.tab_id == "k2" && o.part.is_some()).map(|o| (o.result_id.clone(), o.sql.clone())).collect();
    assert_eq!(parts.len(), 2, "{parts:?}");
    assert_ne!(parts[0].0, parts[1].0, "two results, not one replaced");
    for (rid, sql) in &parts {
        let rs = s.st.engine.results().get(rid).unwrap();
        let mut g = rs.lock();
        let page = g.page(&databrain_result_store::ViewSpec::default(), 0, 10).unwrap();
        let want = if sql.contains("id = 1") { "1" } else { "3" };
        assert_eq!(page.rows.iter().map(|r| r[0].clone().unwrap_or_default()).collect::<Vec<_>>(), [want], "{sql}");
    }
}
