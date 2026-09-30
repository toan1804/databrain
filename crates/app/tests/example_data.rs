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
        let sql = api::file_scan_sql(&data(file)).unwrap();
        let r = query(&st, &id, &sql).await;
        assert_eq!(r.len(), rows, "{file}: {sql}");
    }
    // Direct glob over the partition folder, without attaching anything.
    let glob = format!("SELECT count(*) FROM '{}/*/*.parquet'", data("orders_by_year"));
    assert_eq!(cell(&query(&st, &id, &glob).await, 0, 0), "3000");
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
