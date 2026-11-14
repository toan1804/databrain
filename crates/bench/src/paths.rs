//! The critical paths, by the crate that owns them. Each entry says what
//! the user is doing when it runs. Its budget (ms) is about 15 to 20
//! times what it took on a 2023 laptop when it was added: slow CI
//! machines pass, a path that became quadratic or parses a statement many
//! times does not. Lower a budget when a path gets faster for good.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;

use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::arrow::array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use databrain_connector_core::sql::{self, ParamValue};
use databrain_connector_core::{ConnectionConfig, ConnectorKind, ConnectorRegistry, DbObject, ObjectKind, tables};
use databrain_export::{ExportFormat, ExportOptions};
use databrain_query_engine::{EventSink, JobEvent, QueryEngine, RunRequest, RunResponse};
use databrain_result_store::{ColumnFilter, FilterOp, ResultSet, ResultStore, SortKey, ViewSpec};
use databrain_workspace::{ConnectionProfile, EnvTag, HistoryQuery, NewHistory, Origin, Workspace};

use crate::harness::Suite;

/// A query of the size people write: a CTE, joins, a window, a CASE.
const QUERY: &str = "\
with recent as (
    select o.id, o.customer_id, o.total, o.created_at
    from sales.orders o
    where o.created_at >= date '2026-01-01' and o.status in ('paid', 'shipped') -- this year's
)
select c.name, c.country, count(*) as orders, sum(r.total) as revenue,
       rank() over (partition by c.country order by sum(r.total) desc) as place,
       case when sum(r.total) > 10000 then 'key' else 'regular' end as tier
from recent r
join crm.customers c on c.id = r.customer_id
left join crm.segments s on s.customer_id = c.id and s.name = 'it''s vip'
where c.deleted_at is null and coalesce(s.weight, 1) > 0
group by c.name, c.country
order by revenue desc
limit 100";

/// A script of `n` statements: reads, writes, DDL, comments and strings.
fn script(n: usize) -> String {
    let pieces = [
        "select id, name from app.users where email like '%@example.com' and created_at > now() - interval '7 days'",
        "update app.users set last_seen = now(), note = 'it''s; fine' where id = 42",
        "insert into audit.log (who, what) values ('me', 'ran -- not a comment'), ('you', 'x')",
        "/* monthly */ create table if not exists tmp.report as select country, sum(total) from sales.orders group by 1",
        "delete from tmp.report where country is null -- cleanup",
    ];
    (0..n).map(|i| pieces[i % pieces.len()]).collect::<Vec<_>>().join(";\n")
}

/// `rows` rows in batches of 2000: id, name, city, amount, note (some null).
fn batches(rows: usize) -> (SchemaRef, Vec<RecordBatch>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("city", DataType::Utf8, false),
        Field::new("amount", DataType::Float64, false),
        Field::new("note", DataType::Utf8, true),
    ]));
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < rows {
        let n = 2000.min(rows - at);
        // A fixed scramble, so sorting has work to do.
        let mix = |i: usize| (i.wrapping_mul(2_654_435_761)) % 1_000_003;
        let ids: Vec<i64> = (at..at + n).map(|i| i as i64).collect();
        let names: Vec<String> = (at..at + n).map(|i| format!("customer {:07}", mix(i))).collect();
        let cities: Vec<String> = (at..at + n).map(|i| format!("city {}", mix(i) % 50)).collect();
        let amounts: Vec<f64> = (at..at + n).map(|i| mix(i) as f64 / 7.0).collect();
        let notes: Vec<Option<String>> = (at..at + n).map(|i| (i % 3 != 0).then(|| format!("note about order {i}, \"quoted\", with a comma"))).collect();
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(ids)),
            Arc::new(StringArray::from(names)),
            Arc::new(StringArray::from(cities)),
            Arc::new(Float64Array::from(amounts)),
            Arc::new(StringArray::from(notes)),
        ];
        out.push(RecordBatch::try_new(schema.clone(), cols).expect("batch"));
        at += n;
    }
    (schema, out)
}

pub fn register(s: &mut Suite) {
    sql_text(s);
    redaction(s);
    ai_and_lineage(s);
    results(s);
    export_and_load(s);
    workspace(s);
    engine(s);
}

/// connector-core: read on every keystroke (splitting, the statement at
/// the cursor) and before every run (classification, parameters).
fn sql_text(s: &mut Suite) {
    use ConnectorKind::*;
    s.group("sql");
    let big = script(1000);
    let kb = big.len() as f64 / 1024.0;
    s.add("split_1000_statements", "the editor finds the statement at the cursor in a long script", 15, Some((kb, "KB")), {
        let big = big.clone();
        move || sql::split_statements(&big, Postgres)
    });
    for (name, kind) in [("postgres", Postgres), ("mysql", Mysql), ("mssql", Mssql), ("snowflake", Snowflake)] {
        s.add(&format!("classify_query_{name}"), "a statement is classified before it runs", 1, None, move || sql::classify(QUERY, kind));
    }
    s.add("classify_unparsed", "a statement the parser can't read falls back to keywords", 1, None, || {
        sql::classify("selec * fro t where x = 'a' and y in (select 1 fro u)", Postgres)
    });
    let medium = script(100);
    s.add("plan_100_statements", "a 100-statement script is split and classified to run", 15, Some((100.0, "statements")), {
        let medium = medium.clone();
        move || sql::split_statements(&medium, Postgres).into_iter().map(|p| sql::Checked::new(p.sql, Postgres)).collect::<Vec<_>>()
    });
    s.add("parse_script_100", "the AI policy reads a 100-statement script once", 15, Some((100.0, "statements")), {
        let medium = medium.clone();
        move || sql::parse_script(&medium, Postgres)
    });
    // A session statement: the rule reads its text (a read passes at once).
    let set = "set statement_timeout = 5000";
    let c = sql::classify(set, Postgres);
    s.add("read_only_allows", "a read-only connection checks a SET statement", 1, None, move || sql::read_only_allows(std::hint::black_box(set), &c));
    s.add("unknown_functions", "the AI policy looks for functions that could write", 1, None, || sql::unknown_functions(QUERY, Postgres));
    s.add("table_refs", "table access rules read which tables a query touches", 1, None, || tables::table_refs(QUERY, Postgres));
    let with_params = format!("{QUERY} offset :skip /* :not_one */ -- and 'x' = :label");
    let params: HashMap<String, ParamValue> =
        [("skip", "20"), ("label", "o'brien")].into_iter().map(|(k, v)| (k.to_string(), ParamValue { value: v.into(), raw: false })).collect();
    s.add("bind_parameters", "`:name` parameters are filled in before a run", 1, None, {
        let (q, p) = (with_params.clone(), params);
        move || sql::substitute_parameters(&q, Postgres, &p)
    });
    s.add("find_parameters", "the editor shows a box for each `:name` as the text changes", 1, None, move || sql::find_parameters(&with_params, Postgres));
}

/// What is kept of every statement, job log line and AI message.
fn redaction(s: &mut Suite) {
    s.group("redact");
    s.add("query", "a statement is saved to history", 1, None, || sql::redact_text(QUERY));
    s.add("create_user", "a statement with a secret is saved to history", 1, None, || {
        sql::redact_text("create user app identified by 's3cret'; alter role r password 'x'; select * from s3('https://b/k', 'id', 'key')")
    });
    let big = script(1000);
    let kb = big.len() as f64 / 1024.0;
    s.add("script_1000_statements", "a long script or AI transcript is saved", 100, Some((kb, "KB")), move || sql::redact_text(&big));
    // Text that once made a rule read on to the end each time.
    for (name, piece) in [("repeat_password_for", "password for x "), ("repeat_open_call", "s3("), ("repeat_open_quote", "'set ")] {
        let text = piece.repeat(100 * 1024 / piece.len());
        s.add(&format!("hostile_{name}"), "100 KB built to be slow stays linear", 300, Some((100.0, "KB")), move || sql::redact_secrets(&text));
    }
    let url = "connect with postgres://app:pa/ss@db.internal:5432/sales?sslmode=require or Server=h;Password=pw; then run it ".repeat(100);
    s.add("urls_and_key_values", "an AI message with connection strings is saved", 20, Some((url.len() as f64 / 1024.0, "KB")), move || sql::redact_text(&url));
}

fn profile(kind: ConnectorKind, read_only: bool) -> ConnectionProfile {
    let mut config = ConnectionConfig::new(kind, AuthMethod::None);
    config.read_only = read_only;
    ConnectionProfile {
        id: "bench".into(),
        name: "Bench".into(),
        config,
        color: None,
        env: EnvTag::None,
        folder_id: None,
        has_secret: false,
        ai_policy: Default::default(),
        created_at: 0,
        updated_at: 0,
    }
}

fn ai_and_lineage(s: &mut Suite) {
    s.group("ai");
    let p = profile(ConnectorKind::Postgres, false);
    s.add("review_query", "the assistant's SQL is checked against the connection's policy", 1, None, {
        let p = p.clone();
        move || databrain_ai::policy::review_sql(&p, QUERY)
    });
    let ro = profile(ConnectorKind::Postgres, true);
    s.add("review_query_read_only", "the same on a read-only connection", 1, None, move || databrain_ai::policy::review_sql(&ro, QUERY));
    let medium = script(100);
    s.add("review_script_100", "the assistant proposes a 100-statement script", 15, Some((100.0, "statements")), move || {
        databrain_ai::policy::review_sql(&p, &medium)
    });

    s.group("lineage");
    s.add("query", "lineage of the statement in the editor", 2, None, || {
        databrain_lineage::analyze(QUERY, ConnectorKind::Postgres, &databrain_lineage::NoCatalog)
    });
    let medium = script(100);
    s.add("script_100", "lineage of a 100-statement script", 30, Some((100.0, "statements")), move || {
        databrain_lineage::analyze(&medium, ConnectorKind::Postgres, &databrain_lineage::NoCatalog)
    });
}

/// result-store: the grid over a result in memory.
fn results(s: &mut Suite) {
    s.group("results");
    const ROWS: usize = 200_000;
    let (schema, data) = batches(ROWS);
    let rows = Some((ROWS as f64, "rows"));
    s.add("append_100_batches", "a result's batches are added as they arrive", 1, None, {
        let (schema, data) = (schema.clone(), data.clone());
        move || {
            let mut r = ResultSet::new("r", schema.clone());
            for b in &data {
                r.push(b.clone());
            }
            r.finish(false);
            r.total_rows()
        }
    });
    let set = {
        let mut r = ResultSet::new("r", schema);
        for b in data {
            r.push(b);
        }
        r.finish(false);
        Arc::new(std::sync::Mutex::new(r))
    };
    let plain = ViewSpec::default();
    s.add("page", "the grid scrolls to another page of 200 rows", 2, None, {
        let (set, mut at) = (set.clone(), 0usize);
        move || {
            at = (at + 200) % (ROWS - 200);
            set.lock().expect("lock").page(&plain, at, 200).expect("page").row_ids.len()
        }
    });
    // A view is cached per spec: two specs in turn, so each is computed.
    let turn = |a: ViewSpec, b: ViewSpec| {
        let mut flip = false;
        move || {
            flip = !flip;
            if flip { a.clone() } else { b.clone() }
        }
    };
    let sort = |column, descending| ViewSpec { sort: vec![SortKey { column, descending }], ..Default::default() };
    s.add("sort_text_200k", "a click on a text column's header sorts the result", 300, rows, {
        let (set, mut next) = (set.clone(), turn(sort(1, false), sort(1, true)));
        move || set.lock().expect("lock").page(&next(), 0, 200).expect("page").view_rows
    });
    s.add("sort_number_200k", "a click on a number column's header sorts the result", 100, rows, {
        let (set, mut next) = (set.clone(), turn(sort(3, false), sort(3, true)));
        move || set.lock().expect("lock").page(&next(), 0, 200).expect("page").view_rows
    });
    let filter = |value: &str| ViewSpec { filters: vec![ColumnFilter { column: 2, op: FilterOp::Equals, value: value.into() }], ..Default::default() };
    s.add("filter_column_200k", "a column filter narrows the result", 20, rows, {
        let (set, mut next) = (set.clone(), turn(filter("city 7"), filter("city 8")));
        move || set.lock().expect("lock").page(&next(), 0, 200).expect("page").view_rows
    });
    let quick = |q: &str| ViewSpec { quick_filter: Some(q.into()), ..Default::default() };
    s.add("quick_filter_200k", "typing in the filter box searches every cell", 400, rows, {
        let (set, mut next) = (set.clone(), turn(quick("order 1999"), quick("customer 00012")));
        move || set.lock().expect("lock").page(&next(), 0, 200).expect("page").view_rows
    });
    s.add("find_200k", "Find in the result", 400, rows, {
        let set = set.clone();
        let spec = ViewSpec::default();
        move || set.lock().expect("lock").find(&spec, "order 19999", 1000).is_ok()
    });
    s.add("column_stats_200k", "the statistics of a column", 1000, rows, {
        let spec = ViewSpec::default();
        let mut col = 0;
        move || {
            col = if col == 3 { 1 } else { 3 };
            set.lock().expect("lock").column_stats(&spec, col).is_ok()
        }
    });
    // The store hands results out by id on every grid request.
    let store = ResultStore::new();
    let (schema, data) = batches(2000);
    store.insert_complete("r1", schema, data, false);
    s.add("store_lookup", "every grid request finds its result by id", 1, None, move || store.get("r1").is_ok());
}

fn export_and_load(s: &mut Suite) {
    s.group("export");
    const ROWS: usize = 100_000;
    let (schema, data) = batches(ROWS);
    let rows = Some((ROWS as f64, "rows"));
    for (name, format, budget) in [
        ("csv", ExportFormat::Csv, 700),
        ("json", ExportFormat::Json, 1400),
        ("sql_insert", ExportFormat::SqlInsert, 1500),
        ("markdown", ExportFormat::Markdown, 1500),
    ] {
        let (schema, data) = (schema.clone(), data.clone());
        let mut opts = ExportOptions::new(format);
        opts.table_name = Some("app.orders".into());
        s.add(&format!("{name}_100k_rows"), "a result is exported or copied", budget, rows, move || {
            databrain_export::export_to_string(schema.clone(), &data, opts.clone()).map(|t| t.len()).expect("export")
        });
    }
    for (name, format, budget) in [("parquet", ExportFormat::Parquet, 500), ("xlsx", ExportFormat::Xlsx, 6000)] {
        let (schema, data) = (schema.clone(), data.clone());
        let path = std::env::temp_dir().join(format!("databrain-bench-{}.{}", std::process::id(), format.extension()));
        s.add(&format!("{name}_100k_rows"), "a result is exported to a file", budget, rows, move || {
            let n = databrain_export::export_file(&path, schema.clone(), &data, ExportOptions::new(format)).expect("export");
            let _ = std::fs::remove_file(&path);
            n
        });
    }
    s.group("load");
    s.add("insert_sql_100k_rows", "a load writes its INSERT statements", 1500, rows, move || {
        databrain_export::load::insert_statements(ConnectorKind::Postgres, "app.orders", &schema, &data).map(|v| v.len()).expect("inserts")
    });
}

/// workspace: the SQLite file behind history and the explorer cache.
fn workspace(s: &mut Suite) {
    s.group("workspace");
    let ws = Arc::new(Workspace::open_in_memory().expect("workspace"));
    let entry = |i: usize| NewHistory {
        origin: Origin::User,
        connection_id: Some("c1".into()),
        sql: format!("select * from app.orders_{} where id = {i} -- {QUERY}", i % 97),
        started_at: i as i64,
        duration_ms: 12,
        rows: Some(10),
        status: None,
        error: None,
        output_handle: None,
        result_id: None,
    };
    for i in 0..20_000 {
        ws.add_history(entry(i)).expect("history");
    }
    s.add("history_add", "a statement that ran is saved (redacted) to history", 2, None, {
        let (ws, mut i) = (ws.clone(), 20_000usize);
        move || {
            i += 1;
            ws.add_history(entry(i)).expect("history")
        }
    });
    s.add("history_recent", "the history panel opens", 5, None, {
        let ws = ws.clone();
        move || ws.list_history(&HistoryQuery { limit: Some(200), ..Default::default() }).expect("history").len()
    });
    s.add("history_search_20k", "a search in 20,000 history entries", 60, None, {
        let ws = ws.clone();
        move || ws.list_history(&HistoryQuery { search: Some("orders_42 revenue".into()), limit: Some(200), ..Default::default() }).expect("history").len()
    });
    let objects: Vec<DbObject> = (0..5000)
        .map(|i| DbObject {
            schema: "app".into(),
            name: format!("table_{i:05}"),
            kind: ObjectKind::Table,
            comment: Some("a table".into()),
            row_estimate: Some(i),
        })
        .collect();
    s.add("explorer_cache_put_5k", "a schema of 5,000 objects is listed and cached", 200, Some((5000.0, "objects")), {
        let ws = ws.clone();
        move || ws.meta_put_schema("c1", "app", &objects).expect("cache")
    });
    s.add("explorer_cache_read_5k", "the explorer opens that schema from the cache", 30, Some((5000.0, "objects")), move || {
        ws.meta_listing("c1", "app").expect("cache").map(|l| l.objects.len())
    });
}

/// Tells the bench when a run has finished.
struct Done(std::sync::Mutex<mpsc::Sender<()>>);

impl EventSink for Done {
    fn emit(&self, event: JobEvent) {
        if matches!(event, JobEvent::JobFinished { .. }) {
            let _ = self.0.lock().expect("lock").send(());
        }
    }
}

/// query-engine, end to end on a SQLite file: plan, guard, session, the
/// driver, the result store, history.
fn engine(s: &mut Suite) {
    s.group("engine");
    let path = std::env::temp_dir().join(format!("databrain-bench-{}-{}.db", std::process::id(), uuid::Uuid::new_v4()));
    let mut reg = ConnectorRegistry::new();
    reg.register(Arc::new(databrain_connector_sqlite::SqliteConnector));
    let ws = Arc::new(Workspace::open_in_memory().expect("workspace"));
    let mut config = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    config.file_path = Some(path.to_string_lossy().into());
    let conn = ws.save_connection(ConnectionProfile { id: String::new(), config, ..profile(ConnectorKind::Sqlite, false) }).expect("connection").id;
    let (tx, rx) = mpsc::channel();
    let engine = QueryEngine::new(reg, ws, Arc::new(MemoryStore::default()), Arc::new(ResultStore::new()), Arc::new(Done(std::sync::Mutex::new(tx))));
    let rx = Arc::new(std::sync::Mutex::new(rx));
    // Run `sql` in a tab of its own to the end, then close the tab (its
    // results and session).
    let run = {
        let (engine, rx) = (engine.clone(), rx.clone());
        move |sql: &str| {
            let req = RunRequest {
                connection_id: conn.clone(),
                tab_id: "bench".into(),
                sql: sql.into(),
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin: Origin::User,
                session_key: None,
                output_name: None,
                params: Default::default(),
                output_masks: None,
            };
            match engine.run(req).expect("run") {
                RunResponse::Started { statements, .. } => {
                    rx.lock().expect("lock").recv_timeout(std::time::Duration::from_secs(120)).expect("the run finishes");
                    engine.close_tab("bench");
                    statements.len()
                }
                RunResponse::NeedsConfirmation { reasons, .. } => panic!("not run: {reasons:?}"),
            }
        }
    };
    run("create table big as with recursive c(x) as (select 1 union all select x + 1 from c where x < 100000) \
         select x as id, 'customer ' || x as name, x * 1.5 as amount, 'note about order ' || x as note from c");
    s.add("run_select_1", "the smallest run: what DataBrain adds to a statement", 5, None, {
        let run = run.clone();
        move || run("select 1")
    });
    s.add("run_select_100k_rows", "a query's 100,000 rows arrive in the grid's store", 600, Some((100_000.0, "rows")), {
        let run = run.clone();
        move || run("select * from big")
    });
    let many = (0..200).map(|i| format!("select {i} as n, 'x' as s")).collect::<Vec<_>>().join(";\n");
    s.add("run_script_200", "a script of 200 small statements runs", 400, Some((200.0, "statements")), move || run(&many));
    for suffix in ["", "-wal", "-shm", "-journal"] {
        s.temp_files.push(std::path::PathBuf::from(format!("{}{suffix}", path.display())));
    }
}
