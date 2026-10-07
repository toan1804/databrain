//! End-to-end tests over the sample dataset in `examples/data` (CSV,
//! Parquet, a Hive-partitioned Parquet folder and a SQLite database),
//! exercised through the same app API the desktop UI uses.
//!
//! Regenerate the data with `examples/data/generate.sh`.

use std::path::PathBuf;
use std::sync::Arc;

use databrain_app::api::{self, AppState, SaveConnectionArgs};
use databrain_auth::{AuthMethod, MemoryStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind, ObjectKind};
use databrain_export::{ExportFormat, ExportOptions};
use databrain_query_engine::{EventSink, JobEvent, RunRequest};
use databrain_result_store::{ColumnFilter, FilterOp, SortKey, ViewSpec};
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

fn data(file: &str) -> String {
    let p: PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "..", "examples", "data", file].iter().collect();
    let p = p.canonicalize().unwrap_or_else(|_| panic!("missing example file {file}; run examples/data/generate.sh"));
    p.to_string_lossy().into_owned()
}

fn state() -> AppState {
    AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi))
}

fn add_connection(st: &AppState, name: &str, config: ConnectionConfig) -> String {
    api::save_connection(
        st,
        SaveConnectionArgs {
            profile: ConnectionProfile {
                id: String::new(),
                name: name.into(),
                config,
                color: None,
                env: EnvTag::None,
                folder_id: None,
                has_secret: false,
                ai_policy: Default::default(),
                created_at: 0,
                updated_at: 0,
            },
            secret: None,
            clear_secret: false,
            extra_secrets: Default::default(),
        },
    )
    .unwrap()
    .id
}

fn sqlite_conn(st: &AppState) -> String {
    let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
    c.file_path = Some(data("shop.db"));
    c.read_only = true; // never modify the committed sample database
    add_connection(st, "shop.db", c)
}

fn duck_conn(st: &AppState) -> String {
    let mut c = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
    let files = ["customers.csv", "products.csv", "orders.parquet", "order_items.parquet", "orders_by_year"].map(data);
    c.options.insert("files".into(), files.join("\n"));
    add_connection(st, "Local files", c)
}

/// Run SQL and return (result id, header, rows) of the last result set.
async fn query_view(st: &AppState, conn: &str, sql: &str, view: ViewSpec) -> (String, Vec<String>, Vec<Vec<Option<String>>>) {
    let out = st
        .engine
        .run_and_wait(
            &st.hub,
            RunRequest {
                connection_id: conn.into(),
                tab_id: uuid::Uuid::new_v4().to_string(),
                sql: sql.into(),
                base_offset: 0,
                row_limit: None,
                confirmed: true,
                origin: Default::default(),
                session_key: None,
                output_name: None,
                params: Default::default(),
            },
            None,
        )
        .await
        .unwrap();
    if let Some(e) = out.iter().find_map(|o| o.error.as_ref()) {
        panic!("query failed: {}\n{sql}", e.message);
    }
    let info = out.iter().rev().find_map(|o| o.result.clone()).expect("no result set");
    let page = api::fetch_page(st, info.id.clone(), view, 0, 5000).await.unwrap();
    (info.id, info.columns.iter().map(|c| c.name.clone()).collect(), page.rows)
}

async fn query(st: &AppState, conn: &str, sql: &str) -> Vec<Vec<Option<String>>> {
    query_view(st, conn, sql, ViewSpec::default()).await.2
}

fn cell(rows: &[Vec<Option<String>>], r: usize, c: usize) -> &str {
    rows[r][c].as_deref().unwrap_or("NULL")
}

fn num(s: &str) -> f64 {
    s.parse().unwrap_or_else(|_| panic!("not a number: {s}"))
}

const REVENUE_SQLITE: &str = "SELECT country, orders, revenue FROM revenue_by_country ORDER BY country";
const REVENUE_DUCKDB: &str = "SELECT c.country, count(DISTINCT o.order_id) AS orders, round(sum(o.total_amount), 2) AS revenue
    FROM files.orders o JOIN files.customers c USING (customer_id)
    WHERE o.status <> 'cancelled' GROUP BY c.country ORDER BY c.country";

// ------------------------------------------------------------------ SQLite

#[tokio::test]
async fn sqlite_explorer_and_queries() {
    let st = state();
    let id = sqlite_conn(&st);

    let schemas = api::list_schemas(&st, &id).await.unwrap();
    assert_eq!(schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["main"]);
    let hits = api::search_objects(&st, &id, "order", None).await.unwrap();
    assert_eq!(hits.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["orders", "order_items"]);
    assert!(api::search_objects(&st, &id, "  ", None).await.unwrap().is_empty());
    let objects = api::list_objects(&st, &id, "main").await.unwrap();
    let names = |k: ObjectKind| objects.iter().filter(|o| o.kind == k).map(|o| o.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(ObjectKind::Table), vec!["customers", "order_items", "orders", "products"]);
    assert_eq!(names(ObjectKind::View), vec!["revenue_by_country"]);

    let orders = api::describe(&st, &id, "main", "orders").await.unwrap();
    assert!(orders.columns.iter().find(|c| c.name == "order_id").unwrap().is_primary_key);
    assert!(!orders.columns.iter().find(|c| c.name == "customer_id").unwrap().nullable);
    assert_eq!(orders.foreign_keys.len(), 1);
    assert_eq!(orders.foreign_keys[0].ref_table, "customers");
    assert!(orders.ddl.as_deref().unwrap().contains("CHECK (status IN"));

    // Query hints: primary key + secondary index of the sample database.
    let layout = api::table_layout(&st, &id, "main", "orders").await.unwrap();
    assert_eq!(layout.indexes[0].columns, vec!["order_id"]);
    assert!(layout.indexes.iter().any(|i| i.name == "orders_customer" && i.columns == vec!["customer_id"]), "{layout:?}");
    // Browsing fills the metadata cache: completion answers locally without the AI index.
    st.engine.workspace().meta_clear(&id).unwrap();
    assert!(api::complete_tables_local(&st, &id, None, "ord", 10).unwrap().is_empty());
    api::list_objects(&st, &id, "main").await.unwrap();
    let local = api::complete_tables_local(&st, &id, Some("main"), "ord", 10).unwrap();
    assert_eq!(local.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["orders", "order_items"]);
    assert!(api::complete_columns_local(&st, &id, "main", "customers").unwrap().is_none());
    api::describe(&st, &id, "main", "customers").await.unwrap();
    assert!(api::complete_columns_local(&st, &id, "main", "customers").unwrap().unwrap().contains(&"customer_id".to_string()));
    // Completion searches one schema on the server, best matches first.
    let hits = api::complete_tables(&st, &id, Some("main"), "ORD", 10).await.unwrap();
    assert_eq!(hits.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["orders", "order_items"]);
    assert!(api::complete_tables(&st, &id, None, "", 10).await.unwrap().is_empty());
    // Query tips after a run: references as written in SQL are resolved.
    let hl = databrain_app::ai_api::hint_layouts(&st, &id, vec!["ORDERS".into(), "main.customers".into(), "nope".into()]).await.unwrap();
    assert_eq!((hl[0].schema.as_deref(), hl[0].name.as_deref()), (Some("main"), Some("orders")));
    assert!(hl[0].layout.as_ref().unwrap().indexes.iter().any(|i| i.name == "orders_customer"));
    assert_eq!(hl[1].name.as_deref(), Some("customers"));
    assert!(hl[2].layout.is_none() && hl[2].error.is_some());

    let counts = query(
        &st,
        &id,
        "SELECT (SELECT count(*) FROM customers), (SELECT count(*) FROM products), (SELECT count(*) FROM orders),
                (SELECT count(*) FROM order_items), (SELECT count(*) FROM customers WHERE email IS NULL)",
    )
    .await;
    assert_eq!(counts[0].iter().map(|v| v.as_deref().unwrap()).collect::<Vec<_>>(), vec!["250", "18", "3000", "7417", "14"]);

    // Unicode, and quoted commas/quotes survive.
    let r = query(&st, &id, "SELECT count(*) FROM customers WHERE city = 'Hà Nội'").await;
    assert!(num(cell(&r, 0, 0)) > 0.0);
    let r = query(&st, &id, "SELECT name FROM products WHERE name LIKE '%\"Slim\"%'").await;
    assert_eq!(cell(&r, 0, 0), "Accessory Case \"Slim\"");

    // Order totals equal the sum of their items.
    let r = query(&st, &id, "SELECT round(sum(total_amount), 2), (SELECT round(sum(amount), 2) FROM order_items) FROM orders").await;
    assert_eq!(cell(&r, 0, 0), cell(&r, 0, 1));

    // Grid view: filter + sort on a multi-statement script's last result.
    let view = ViewSpec {
        filters: vec![ColumnFilter { column: 0, op: FilterOp::Equals, value: "Vietnam".into() }],
        quick_filter: None,
        sort: vec![SortKey { column: 1, descending: false }],
    };
    let (_, header, rows) = query_view(
        &st,
        &id,
        "SELECT 1; SELECT country, city, count(*) AS n FROM customers GROUP BY 1, 2",
        view,
    )
    .await;
    assert_eq!(header, vec!["country", "city", "n"]);
    assert_eq!(rows.iter().map(|r| r[1].clone().unwrap()).collect::<Vec<_>>(), vec!["Hà Nội", "Hồ Chí Minh", "Đà Nẵng"]);

    // Read-only connection blocks writes before they reach the database.
    let blocked = st.engine.run(RunRequest {
        connection_id: id.clone(),
        tab_id: "w".into(),
        sql: "DELETE FROM orders".into(),
        base_offset: 0,
        row_limit: None,
        confirmed: true,
        origin: Default::default(),
        session_key: None,
        output_name: None,
        params: Default::default(),
    });
    assert!(blocked.is_err(), "write on a read-only connection must be rejected");
}

// ------------------------------------------------------------------ DuckDB over CSV + Parquet

#[tokio::test]
async fn duckdb_attached_csv_and_parquet_files() {
    let st = state();
    let id = duck_conn(&st);

    let schemas = api::list_schemas(&st, &id).await.unwrap();
    let files = schemas.iter().find(|s| s.name.ends_with(".files")).expect("files schema").name.clone();
    let objects = api::list_objects(&st, &id, &files).await.unwrap();
    assert_eq!(
        objects.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(),
        vec!["customers", "order_items", "orders", "orders_by_year", "products"]
    );
    assert!(objects.iter().all(|o| o.kind == ObjectKind::View));

    // Types inferred from CSV and read from Parquet metadata.
    let ty = |d: &databrain_connector_core::ObjectDetail, c: &str| d.columns.iter().find(|x| x.name == c).unwrap().data_type.clone();
    let customers = api::describe(&st, &id, &files, "customers").await.unwrap();
    assert_eq!(ty(&customers, "customer_id"), "BIGINT");
    assert_eq!(ty(&customers, "signup_date"), "DATE");
    assert_eq!(ty(&customers, "is_vip"), "BOOLEAN");
    let orders = api::describe(&st, &id, &files, "orders").await.unwrap();
    assert_eq!(ty(&orders, "ordered_at"), "TIMESTAMP");
    assert_eq!(ty(&orders, "total_amount"), "DECIMAL(12,2)");
    assert!(orders.object.comment.as_deref().unwrap().ends_with("orders.parquet"));

    let r = query(
        &st,
        &id,
        "SELECT (SELECT count(*) FROM files.customers), (SELECT count(*) FROM files.products),
                (SELECT count(*) FROM files.orders), (SELECT count(*) FROM files.order_items),
                (SELECT count(*) FROM files.customers WHERE email IS NULL)",
    )
    .await;
    assert_eq!(r[0].iter().map(|v| v.as_deref().unwrap()).collect::<Vec<_>>(), vec!["250", "18", "3000", "7417", "14"]);

    // CSV quoting: embedded comma and doubled quotes.
    let r = query(&st, &id, "SELECT name FROM files.customers WHERE customer_id = 50").await;
    assert!(cell(&r, 0, 0).contains(", "), "{r:?}");
    let r = query(&st, &id, "SELECT name FROM files.products WHERE product_id = 15").await;
    assert_eq!(cell(&r, 0, 0), "Accessory Case \"Slim\"");

    // Hive-partitioned folder: partition column + pruning.
    let r = query(&st, &id, "SELECT year, count(*) FROM files.orders_by_year GROUP BY year ORDER BY year").await;
    assert_eq!(r.iter().map(|x| x[0].clone().unwrap()).collect::<Vec<_>>(), vec!["2023", "2024", "2025"]);
    assert_eq!(r.iter().map(|x| num(x[1].as_deref().unwrap())).sum::<f64>(), 3000.0);

    // Window function + CSV/Parquet join.
    let r = query(
        &st,
        &id,
        "SELECT p.category, sum(i.amount) AS revenue, rank() OVER (ORDER BY sum(i.amount) DESC) AS rnk
         FROM files.order_items i JOIN files.products p USING (product_id) GROUP BY 1 ORDER BY rnk",
    )
    .await;
    assert_eq!(r.len(), 4);
    assert_eq!(cell(&r, 0, 2), "1");
}

#[tokio::test]
async fn same_answer_from_sqlite_and_from_csv_parquet() {
    let st = state();
    let (sq, dk) = (sqlite_conn(&st), duck_conn(&st));
    let a = query(&st, &sq, REVENUE_SQLITE).await;
    let b = query(&st, &dk, REVENUE_DUCKDB).await;
    assert_eq!(a.len(), 7); // 7 countries
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x[0], y[0]);
        assert_eq!(x[1], y[1]);
        assert!((num(x[2].as_deref().unwrap()) - num(y[2].as_deref().unwrap())).abs() < 0.005, "{x:?} vs {y:?}");
    }
}

// ------------------------------------------------------------------ "Query a local file…"

#[tokio::test]
async fn query_local_file_suggestions_run() {
    let st = state();
    let id = add_connection(&st, "scratch", ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None));
    for (file, rows) in [("customers.csv", 250), ("products.csv", 18), ("orders.parquet", 1000), ("order_items.parquet", 1000), ("orders_by_year", 1000)] {
        let sql = api::file_scan_sql(&data(file), false).unwrap();
        let r = query(&st, &id, &sql).await;
        assert_eq!(r.len(), rows, "{file}: {sql}");
    }
    // Direct glob over the partition folder, without attaching anything.
    let glob = format!("SELECT count(*) FROM '{}/*/*.parquet'", data("orders_by_year"));
    assert_eq!(cell(&query(&st, &id, &glob).await, 0, 0), "3000");
}

/// Excel workbook with several sheets and a blank header cell: first sheet by
/// default, every sheet on request, all columns read.
#[tokio::test]
async fn query_local_excel_sheets() {
    let ext = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/duckdb-extensions");
    if ext.is_dir() {
        databrain_connector_duckdb::set_extension_dir(Some(ext.to_string_lossy().into_owned()));
    }
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("book.xlsx");
    let mut wb = rust_xlsxwriter::Workbook::new();
    let a = wb.add_worksheet().set_name("Orders").unwrap();
    a.write_string(0, 0, "id").unwrap();
    a.write_string(0, 2, "total").unwrap(); // B1 blank
    for r in 1..=3u32 {
        a.write_number(r, 0, r as f64).unwrap();
        a.write_string(r, 1, "note").unwrap();
        a.write_number(r, 2, 10.0 * r as f64).unwrap();
    }
    let b = wb.add_worksheet().set_name("Customers").unwrap();
    b.write_string(0, 0, "name").unwrap();
    b.write_string(1, 0, "Ann").unwrap();
    wb.save(&p).unwrap();
    let path = p.to_string_lossy().to_string();

    let sheets = api::excel_sheets(&path).unwrap();
    assert_eq!(sheets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["Orders", "Customers"]);

    let st = state();
    let id = add_connection(&st, "scratch", ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None));
    let first = api::file_scan_sql(&path, false).unwrap();
    assert!(!first.contains("Customers"), "{first}");
    let out = run_named(&st, &id, "x1", &first, None, None).await;
    assert_eq!(out.len(), 1);
    let res = out[0].result.as_ref().unwrap_or_else(|| panic!("{:?}", out[0].error));
    assert_eq!(res.total_rows, 3);
    let cols: Vec<&str> = res.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, vec!["id", "column_B", "total"], "{first}");

    let all = api::file_scan_sql(&path, true).unwrap();
    assert!(all.contains("-- Sheet: Orders") && all.contains("-- Sheet: Customers"), "{all}");
    let out = run_named(&st, &id, "x2", &all, None, None).await;
    assert_eq!(out.len(), 2, "one result per sheet");
    assert!(out.iter().all(|o| o.error.is_none()), "{out:?}");
    assert_eq!(out[1].result.as_ref().unwrap().total_rows, 1);
}

// ------------------------------------------------------------------ export round trip

#[tokio::test]
async fn export_sqlite_result_and_read_back_with_duckdb() {
    let st = state();
    let (sq, dk) = (sqlite_conn(&st), add_connection(&st, "scratch", ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)));
    let (result_id, _, rows) = query_view(&st, &sq, "SELECT * FROM orders ORDER BY order_id", ViewSpec::default()).await;
    assert_eq!(rows.len(), 3000);
    let dir = tempfile::tempdir().unwrap();
    for (fmt, ext) in [(ExportFormat::Parquet, "parquet"), (ExportFormat::Csv, "csv"), (ExportFormat::Xlsx, "xlsx")] {
        let path = dir.path().join(format!("orders.{ext}")).to_string_lossy().into_owned();
        let opts = ExportOptions { format: fmt, header: true, table_name: None, dialect: None, columns: vec![] };
        let n = api::export_result(&st, result_id.clone(), ViewSpec::default(), opts, path.clone()).await.unwrap();
        assert_eq!(n, 3000, "{ext}");
        if ext == "xlsx" {
            // Valid OOXML package (zip); reading it back needs DuckDB's excel extension (network).
            assert_eq!(&std::fs::read(&path).unwrap()[..2], b"PK");
            continue;
        }
        let reader = if ext == "parquet" { "read_parquet" } else { "read_csv" };
        let r = query(&st, &dk, &format!("SELECT count(*), round(sum(total_amount), 2), min(order_id), max(order_id) FROM {reader}('{path}')")).await;
        let orig = query(&st, &sq, "SELECT count(*), round(sum(total_amount), 2), min(order_id), max(order_id) FROM orders").await;
        assert_eq!(cell(&r, 0, 0), "3000");
        assert!((num(cell(&r, 0, 1)) - num(cell(&orig, 0, 1))).abs() < 0.005, "{ext}: {r:?} vs {orig:?}");
        assert_eq!((cell(&r, 0, 2), cell(&r, 0, 3)), ("1", "3000"));
    }
}

// ------------------------------------------------------------------ outputs (handles, results.*, diff, chart, pin)

async fn run_named(st: &AppState, conn: &str, tab: &str, sql: &str, limit: Option<usize>, name: Option<&str>) -> Vec<databrain_query_engine::StatementOutcomeView> {
    let out = st
        .engine
        .run_and_wait(
            &st.hub,
            RunRequest {
                connection_id: conn.into(),
                tab_id: tab.into(),
                sql: sql.into(),
                base_offset: 0,
                row_limit: limit,
                confirmed: true,
                origin: Default::default(),
                session_key: None,
                output_name: name.map(str::to_string),
                params: Default::default(),
            },
            None,
        )
        .await
        .unwrap();
    if let Some(e) = out.iter().find_map(|o| o.error.as_ref()) {
        panic!("query failed: {}\n{sql}", e.message);
    }
    out
}

#[tokio::test]
async fn outputs_handles_cross_source_queries_diff_chart_and_pins() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(Workspace::open(dir.path().join("ws.db")).unwrap());
    let st = AppState::new(ws.clone(), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    st.set_snapshot_dir(dir.path().join("outputs"));
    let (sq, dk) = (sqlite_conn(&st), duck_conn(&st));

    // 1) SQLite output, named `revenue`.
    run_named(&st, &sq, "t1", "SELECT c.country, round(sum(o.total_amount), 2) AS revenue FROM orders o JOIN customers c USING (customer_id) WHERE o.status <> 'cancelled' GROUP BY 1", None, Some("revenue")).await;
    let rev = api::get_output(&st, "revenue").unwrap();
    assert_eq!((rev.handle.as_str(), rev.rows, rev.connection_name.as_str()), ("r1", 7, "shop.db"));
    assert!(rev.sql.contains("GROUP BY 1"));
    // 2) DuckDB-over-files output in another tab: customers per country.
    run_named(&st, &dk, "t2", "SELECT country, count(*) AS customers FROM files.customers GROUP BY 1", None, None).await;
    let cust = api::get_output(&st, "r2").unwrap();
    assert_eq!(cust.rows, 7);

    // 3) Join both outputs in the Results connection (SQLite × CSV).
    let res = api::results_connection(&st).unwrap();
    assert_eq!(api::results_connection(&st).unwrap(), res, "created once");
    let rows = query(&st, &res, "SELECT r.country, round(r.revenue / c.customers, 2) AS per_customer FROM results.revenue r JOIN results.r2 c USING (country) ORDER BY per_customer DESC").await;
    assert_eq!(rows.len(), 7);
    let vn = rows.iter().find(|r| r[0].as_deref() == Some("Vietnam")).unwrap();
    assert!(num(vn[1].as_deref().unwrap()) > 0.0);

    // 4) Re-run tab 1 with a filter → new version; old one is revenue__1.
    run_named(&st, &sq, "t1", "SELECT c.country, round(sum(o.total_amount), 2) AS revenue FROM orders o JOIN customers c USING (customer_id) WHERE o.status = 'delivered' GROUP BY 1", None, Some("revenue")).await;
    assert_eq!(api::get_output(&st, "revenue").unwrap().handle, "r4");
    assert_eq!(api::get_output(&st, "revenue__1").unwrap().handle, "r1");
    // r3 was the Results-connection join; handles count every output.
    assert_eq!(api::get_output(&st, "r3").unwrap().connection_name, "Results (DuckDB)");

    // 5) Diff the two versions by country: every country's revenue changed.
    let diff = api::output_diff_sql(&st, "revenue__1", "revenue", vec!["country".into()], vec![], false).unwrap();
    let rows = query(&st, &res, &diff).await;
    assert_eq!(rows.len(), 7, "{diff}");
    assert!(rows.iter().all(|r| r[0].as_deref() == Some("changed")));
    let keyless = api::output_diff_sql(&st, "revenue__1", "revenue", vec![], vec![], false).unwrap();
    assert_eq!(query(&st, &res, &keyless).await.len(), 14); // 7 removed + 7 added

    // 5b) Different column names: COUNTRY matches country (case), and the
    //     user pairs total ↔ revenue by hand.
    run_named(&st, &sq, "t5", "SELECT c.country AS COUNTRY, round(sum(o.total_amount), 2) AS total FROM orders o JOIN customers c USING (customer_id) WHERE o.status = 'delivered' GROUP BY 1", None, Some("renamed")).await;
    let r = api::output_diff_sql(&st, "renamed", "revenue", vec!["country".into()], vec![], false).unwrap();
    assert!(r.contains("COUNTRY AS country") || r.contains("\"COUNTRY\" AS country"), "{r}");
    let rows = query(&st, &res, &r).await;
    assert!(rows.iter().all(|x| x[0].as_deref() != Some("changed")), "revenue is unmatched, so only keys compare: {r}");
    let mapped = api::output_diff_sql(&st, "renamed", "revenue", vec!["country".into()], vec![("total".into(), "revenue".into())], false).unwrap();
    assert!(mapped.contains("total → revenue"), "{mapped}");
    assert_eq!(query(&st, &res, &mapped).await.len(), 0, "same data under other names: no differences\n{mapped}");
    assert!(api::output_diff_sql(&st, "renamed", "revenue", vec![], vec![("nope".into(), "revenue".into())], false).is_err());
    // Exact mapping: every pair given, COUNTRY ↔ country and total ↔ revenue together.
    let exact = api::output_diff_sql(&st, "renamed", "revenue", vec!["country".into()], vec![("COUNTRY".into(), "country".into()), ("total".into(), "revenue".into())], true).unwrap();
    assert!(exact.contains("COUNTRY → country, total → revenue"), "{exact}");
    assert_eq!(query(&st, &res, &exact).await.len(), 0, "{exact}");

    // 6) Truncated outputs warn when queried.
    run_named(&st, &sq, "t3", "SELECT * FROM orders", Some(100), Some("orders_sample")).await;
    let o = api::get_output(&st, "orders_sample").unwrap();
    assert!(o.truncated && o.rows == 100);
    let out = run_named(&st, &res, "t4", "SELECT count(*) FROM results.orders_sample", None, None).await;
    assert!(out[0].notices.iter().any(|n| n.contains("only the first 100 rows")), "{:?}", out[0].notices);

    // 7) Chart data straight from an output.
    let spec: databrain_result_store::ChartSpec = serde_json::from_value(serde_json::json!({"x": 0, "y": [1], "agg": "sum"})).unwrap();
    let chart = api::chart_data(&st, rev.result_id.clone(), ViewSpec::default(), spec).await.unwrap();
    assert_eq!(chart.x.len(), 7);
    assert_eq!(chart.series[0].name, "revenue");

    // 8) History links to the output.
    let h = ws.list_history(&databrain_workspace::HistoryQuery::default()).unwrap();
    assert!(h.iter().any(|e| e.output_handle.as_deref() == Some("r1")));

    // 9) Pin, "restart", query again.
    api::pin_output(&st, "r4".into(), true).await.unwrap();
    assert!(api::rename_output(&st, "r2", Some("r9".into())).is_err(), "handle-like names are reserved");
    drop(st);
    let st2 = AppState::new(ws.clone(), Arc::new(MemoryStore::default()), Arc::new(Quiet), Arc::new(NoUi));
    st2.set_snapshot_dir(dir.path().join("outputs"));
    let restored = api::get_output(&st2, "revenue").unwrap();
    assert_eq!((restored.handle.as_str(), restored.pinned), ("r4", true));
    assert!(api::get_output(&st2, "r2").is_err(), "unpinned outputs do not survive a restart");
    let res2 = api::results_connection(&st2).unwrap();
    assert_eq!(res2, res);
    let rows = query(&st2, &res2, "SELECT count(*) FROM results.revenue").await;
    assert_eq!(cell(&rows, 0, 0), "7");
    // New outputs continue the numbering.
    run_named(&st2, &sq, "t1", "SELECT 1", None, None).await;
    let handles: Vec<u64> = api::list_outputs(&st2).iter().map(|o| o.handle[1..].parse().unwrap()).collect();
    assert_eq!(handles.iter().filter(|h| **h == 4).count(), 1, "{handles:?}");
    assert!(handles[0] > 8, "{handles:?}");
}
