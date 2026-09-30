//! Converts the staged sample CSVs to Parquet with the bundled DuckDB:
//! `cargo run -p databrain-connector-duckdb --example sample_parquet -- examples/data`
//!
//! Output: `orders.parquet`, `order_items.parquet` (zstd) and a
//! Hive-partitioned folder `orders_by_year/year=YYYY/*.parquet`.

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "examples/data".into());
    let dir = std::path::Path::new(&dir);
    let staging = dir.join("_staging");
    let p = |f: &str| dir.join(f).to_string_lossy().replace('\'', "''");
    let s = |f: &str| staging.join(f).to_string_lossy().replace('\'', "''");
    let _ = std::fs::remove_dir_all(dir.join("orders_by_year"));
    let conn = duckdb::Connection::open_in_memory().expect("duckdb");
    let sql = format!(
        "COPY (SELECT order_id::INTEGER AS order_id, customer_id::INTEGER AS customer_id,
                      ordered_at::TIMESTAMP AS ordered_at, status, total_amount::DECIMAL(12,2) AS total_amount
               FROM read_csv('{orders}') ORDER BY order_id)
           TO '{orders_pq}' (FORMAT parquet, COMPRESSION zstd);
         COPY (SELECT order_item_id::INTEGER AS order_item_id, order_id::INTEGER AS order_id,
                      product_id::INTEGER AS product_id, quantity::SMALLINT AS quantity,
                      unit_price::DECIMAL(10,2) AS unit_price, discount::DOUBLE AS discount,
                      amount::DECIMAL(12,2) AS amount
               FROM read_csv('{items}') ORDER BY order_item_id)
           TO '{items_pq}' (FORMAT parquet, COMPRESSION zstd);
         COPY (SELECT *, year(ordered_at) AS year FROM read_parquet('{orders_pq}'))
           TO '{by_year}' (FORMAT parquet, PARTITION_BY (year), FILENAME_PATTERN 'orders_{{i}}');",
        orders = s("orders.csv"),
        items = s("order_items.csv"),
        orders_pq = p("orders.parquet"),
        items_pq = p("order_items.parquet"),
        by_year = p("orders_by_year"),
    );
    conn.execute_batch(&sql).expect("convert to parquet");
    for t in ["orders.parquet", "order_items.parquet"] {
        let n: i64 = conn.query_row(&format!("SELECT count(*) FROM read_parquet('{}')", p(t)), [], |r| r.get(0)).unwrap();
        println!("{t}: {n} rows");
    }
}
