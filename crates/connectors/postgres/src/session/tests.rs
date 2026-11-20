use super::*;
use crate::PostgresConnector;
use databrain_connector_core::{ConnectionConfig, Connector, SslMode};
use std::sync::Arc;

async fn live_session() -> Option<Box<dyn Session>> {
    let host = std::env::var("DATABRAIN_PG_HOST").ok()?;
    use databrain_auth::{AuthMethod, InlineCredentialSource};
    let user = std::env::var("DATABRAIN_PG_USER").unwrap_or_else(|_| "postgres".into());
    let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: user.clone() });
    cfg.host = Some(host);
    cfg.port = std::env::var("DATABRAIN_PG_PORT").ok().and_then(|p| p.parse().ok());
    cfg.database = std::env::var("DATABRAIN_PG_DB").ok();
    let creds =
        Arc::new(InlineCredentialSource::new(AuthMethod::Password { user }, std::env::var("DATABRAIN_PG_PASSWORD").ok().map(secrecy::SecretString::from)));
    Some(PostgresConnector.connect(&cfg, creds).await.unwrap())
}

/// Live: full table DDL (constraints, partition key, indexes, comments) that recreates the table; function DDL.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_object_ddl() {
    let Some(s) = live_session().await else {
        panic!("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    };
    let s = &s;
    let run = |sql: String| async move {
        s.execute(databrain_connector_core::Statement::internal(&sql), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    for stmt in [
        "drop schema if exists kb_ddl cascade",
        "create schema kb_ddl",
        "create table kb_ddl.customers (id int primary key, email text not null unique)",
        "create table kb_ddl.orders (id bigint generated always as identity, customer_id int not null references kb_ddl.customers(id) on delete cascade, \
           status text default 'new' check (status in ('new','paid')), created_at date not null, primary key (id, created_at)) partition by range (created_at)",
        "create index orders_status_idx on kb_ddl.orders (status) where status <> 'paid'",
        "comment on table kb_ddl.orders is 'Web orders'",
        "comment on column kb_ddl.orders.status is 'it''s the state'",
        "create function kb_ddl.twice(x int) returns int language sql as 'select x * 2'",
    ] {
        run(stmt.to_string()).await;
    }
    let ddl = s.object_ddl("kb_ddl", "orders", ObjectKind::Table).await.unwrap().unwrap();
    for part in [
        "CREATE TABLE kb_ddl.orders",
        "PRIMARY KEY (id, created_at)",
        "FOREIGN KEY (customer_id) REFERENCES kb_ddl.customers(id) ON DELETE CASCADE",
        "CHECK",
        "PARTITION BY RANGE (created_at)",
        "CREATE INDEX orders_status_idx",
        "COMMENT ON TABLE kb_ddl.orders IS 'Web orders'",
        "IS 'it''s the state'",
    ] {
        assert!(ddl.contains(part), "missing {part:?} in\n{ddl}");
    }
    // It runs: drop and recreate from the DDL.
    run("drop table kb_ddl.orders".into()).await;
    for stmt in ddl.split(";\n").map(str::trim).filter(|x| !x.is_empty()) {
        run(stmt.trim_end_matches(';').to_string()).await;
    }
    assert_eq!(s.object_ddl("kb_ddl", "orders", ObjectKind::Table).await.unwrap().unwrap(), ddl, "same DDL after recreating");
    let f = s.object_ddl("kb_ddl", "twice", ObjectKind::Function).await.unwrap().unwrap();
    assert!(f.contains("CREATE OR REPLACE FUNCTION kb_ddl.twice(x integer)"), "{f}");
    assert!(s.object_ddl("kb_ddl", "customers", ObjectKind::Table).await.unwrap().unwrap().contains("UNIQUE (email)"));
    assert!(ddl.contains("id bigint GENERATED ALWAYS AS IDENTITY NOT NULL"), "{ddl}");
    // Names that need quotes keep them.
    run(r#"create table kb_ddl."Mixed Case" ("select" int, "Name" text)"#.into()).await;
    let m = s.object_ddl("kb_ddl", "Mixed Case", ObjectKind::Table).await.unwrap().unwrap();
    assert!(m.starts_with("CREATE TABLE kb_ddl.\"Mixed Case\" (\n    \"select\" integer,\n    \"Name\" text\n)"), "{m}");
    run("drop schema kb_ddl cascade".into()).await;
}

/// Live, Amazon Redshift: DATABRAIN_REDSHIFT_HOST (+ _PORT, _DB, _USER, _PASSWORD).
/// Explorer metadata through the Redshift catalog queries.
#[ignore = "live: needs DATABRAIN_REDSHIFT_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_redshift_metadata() {
    let Ok(host) = std::env::var("DATABRAIN_REDSHIFT_HOST") else {
        panic!("live test: DATABRAIN_REDSHIFT_HOST not set (run with --ignored)");
    };
    use databrain_auth::{AuthMethod, InlineCredentialSource};
    let user = std::env::var("DATABRAIN_REDSHIFT_USER").unwrap_or_else(|_| "awsuser".into());
    let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: user.clone() });
    cfg.host = Some(host);
    cfg.port = std::env::var("DATABRAIN_REDSHIFT_PORT").ok().and_then(|p| p.parse().ok());
    cfg.database = std::env::var("DATABRAIN_REDSHIFT_DB").ok();
    cfg.options.insert("flavor".into(), "redshift".into());
    cfg.ssl_mode = SslMode::Require;
    let creds = Arc::new(InlineCredentialSource::new(
        AuthMethod::Password { user },
        std::env::var("DATABRAIN_REDSHIFT_PASSWORD").ok().map(secrecy::SecretString::from),
    ));
    let s = PostgresConnector.connect(&cfg, creds).await.unwrap();
    assert!(s.server_version().await.unwrap().contains("Redshift"));
    let s = &s;
    let run = |sql: &'static str| async move {
        s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    run("drop schema if exists databrain_rs cascade").await;
    run("create schema databrain_rs").await;
    run("create table databrain_rs.customers (id int primary key, name varchar(40)) diststyle all").await;
    run("create table databrain_rs.orders (id int primary key, customer_id int references databrain_rs.customers(id), at date default current_date) distkey(customer_id) sortkey(at)")
        .await;
    run("comment on table databrain_rs.orders is 'Web orders'").await;
    assert!(s.list_schemas().await.unwrap().iter().any(|x| x.name == "databrain_rs"));
    let objs = s.list_objects("databrain_rs").await.unwrap();
    assert_eq!(objs.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["customers", "orders"]);
    let d = s.describe("databrain_rs", "orders").await.unwrap();
    assert_eq!(d.object.comment.as_deref(), Some("Web orders"));
    assert!(d.columns[0].is_primary_key && d.columns[2].default.is_some());
    assert_eq!(d.foreign_keys[0].ref_table, "customers");
    assert!(d.ddl.unwrap().to_uppercase().contains("CREATE TABLE"));
    let l = s.table_layout("databrain_rs", "orders").await.unwrap();
    assert_eq!(l.cluster_by, vec!["at"]);
    assert!(l.notes.iter().any(|n| n == "DISTKEY (customer_id)"), "{:?}", l.notes);
    assert_eq!(s.schema_columns("databrain_rs").await.unwrap().len(), 2);
    assert!(!s.search_objects("orders", 10).await.unwrap().is_empty());
    let r = run("select current_date as d, 1.5::numeric(5,2) as n").await;
    assert_eq!(r.num_rows(), 1);
    run("drop schema databrain_rs cascade").await;
}

/// Live: routine completion (pg_proc): search path vs a given schema, procedures, ranking.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_search_routines() {
    let Some(s) = live_session().await else {
        panic!("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    };
    for stmt in [
        "drop schema if exists kb_r cascade",
        "drop function if exists public.kb_total(int)",
        "create schema kb_r",
        "create function public.kb_total(x int) returns int language sql as 'select x * 2'",
        "create function kb_r.kb_total_tax(x int) returns int language sql as 'select x'",
        "create procedure kb_r.kb_refresh() language sql as 'select 1'",
    ] {
        s.execute(databrain_connector_core::Statement::internal(stmt), ExecOptions::default()).await.unwrap().collect().await.unwrap();
    }
    let names = |v: Vec<DbObject>| v.into_iter().map(|o| format!("{}.{} {:?}", o.schema, o.name, o.kind)).collect::<Vec<_>>();
    // No schema: the search path only (kb_r is not on it); no pg_catalog functions.
    assert_eq!(names(s.search_routines(None, "KB_", 50).await.unwrap()), vec!["public.kb_total Function"]);
    assert!(s.search_routines(None, "now", 50).await.unwrap().is_empty());
    assert_eq!(names(s.search_routines(Some("kb_r"), "kb", 50).await.unwrap()), vec!["kb_r.kb_refresh Procedure", "kb_r.kb_total_tax Function"]);
    for stmt in ["drop schema kb_r cascade", "drop function public.kb_total(int)"] {
        s.execute(databrain_connector_core::Statement::internal(stmt), ExecOptions::default()).await.unwrap().collect().await.unwrap();
    }
}

/// Live: explorer fingerprints cover routines/sequences; object versions
/// change only for the object that changed; named listing.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_explorer_versions() {
    let Some(s) = live_session().await else {
        panic!("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    };
    let run = |sql: &'static str| async {
        for stmt in sql.split(';').map(str::trim).filter(|x| !x.is_empty()) {
            s.execute(databrain_connector_core::Statement::internal(stmt), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        }
    };
    run("drop schema if exists ex_a cascade; drop schema if exists ex_b cascade").await;
    run("create schema ex_a; create schema ex_b; create table ex_a.t1 (id int primary key); create table ex_a.t2 (x int); \
         create view ex_a.v1 as select * from ex_a.t1; create sequence ex_a.s1; create table ex_b.other (y int)")
    .await;
    let fp = |m: &std::collections::HashMap<String, String>, k: &str| m.get(k).cloned().unwrap();
    let f0 = s.explorer_fingerprints().await.unwrap().unwrap();
    assert_eq!(fp(&f0, "ex_a"), fp(&s.explorer_fingerprints().await.unwrap().unwrap(), "ex_a"), "stable");
    let v0 = s.object_versions("ex_a").await.unwrap().unwrap();
    let mut keys: Vec<&str> = v0.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, vec!["sequence:s1", "table:t1", "table:t2", "view:v1"]);
    // Same objects as the listing.
    let mut listed: Vec<String> = s.list_objects("ex_a").await.unwrap().iter().map(|o| databrain_connector_core::version_key(o.kind, &o.name)).collect();
    listed.sort();
    assert_eq!(listed, keys);

    run("create function ex_a.f(a int) returns int language sql as 'select a'").await;
    let f1 = s.explorer_fingerprints().await.unwrap().unwrap();
    assert_ne!(fp(&f0, "ex_a"), fp(&f1, "ex_a"), "new function");
    assert_eq!(fp(&f0, "ex_b"), fp(&f1, "ex_b"));
    let v1 = s.object_versions("ex_a").await.unwrap().unwrap();
    assert!(v1.contains_key("function:f"));
    assert_eq!(v0["table:t1"], v1["table:t1"]);

    run("alter table ex_a.t2 add column z text; comment on table ex_a.t1 is 'hello'").await;
    let v2 = s.object_versions("ex_a").await.unwrap().unwrap();
    assert_ne!(v1["table:t2"], v2["table:t2"], "new column");
    assert_ne!(v1["table:t1"], v2["table:t1"], "comment");
    assert_eq!(v1["view:v1"], v2["view:v1"], "untouched");
    let f2 = s.explorer_fingerprints().await.unwrap().unwrap();
    run("insert into ex_a.t2 values (1, 'a'); analyze ex_a.t2").await;
    assert_eq!(v2, s.object_versions("ex_a").await.unwrap().unwrap(), "data changes are not definition changes");
    // Row estimates count by power of two: 0 → 1 row changes the
    // schema's fingerprint (its estimates are read again), 1100 → 1200 doesn't.
    let f3 = s.explorer_fingerprints().await.unwrap().unwrap();
    assert_ne!(fp(&f2, "ex_a"), fp(&f3, "ex_a"), "estimate doubled");
    run("insert into ex_a.t2 select g, 'x' from generate_series(2, 1100) g; analyze ex_a.t2").await;
    let f4 = s.explorer_fingerprints().await.unwrap().unwrap();
    run("insert into ex_a.t2 select g, 'x' from generate_series(1101, 1200) g; analyze ex_a.t2").await;
    assert_eq!(fp(&f4, "ex_a"), fp(&s.explorer_fingerprints().await.unwrap().unwrap(), "ex_a"), "a 10% change keeps it");
    assert_eq!(fp(&f3, "ex_b"), fp(&f4, "ex_b"), "other schemas untouched");

    let named = s.list_objects_named("ex_a", &["t2".into(), "f".into()]).await.unwrap();
    let mut n: Vec<String> = named.iter().map(|o| databrain_connector_core::version_key(o.kind, &o.name)).collect();
    n.sort();
    assert_eq!(n, vec!["function:f", "table:t2"]);
    run("drop schema ex_a cascade; drop schema ex_b cascade").await;
}

/// Live: bulk metadata (one batch query) and schema fingerprints for incremental indexing.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_bulk_metadata_and_fingerprints() {
    let Some(s) = live_session().await else {
        panic!("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    };
    let run = |sql: &'static str| async {
        for stmt in sql.split(';').map(str::trim).filter(|x| !x.is_empty()) {
            s.execute(databrain_connector_core::Statement::internal(stmt), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        }
    };
    run("drop schema if exists kb_a cascade; drop schema if exists kb_b cascade; drop schema if exists kb_c cascade").await;
    run("create schema kb_a; create schema kb_b; create schema kb_c; \
         create table kb_a.customers (id int primary key, email text); comment on column kb_a.customers.email is 'login'; \
         comment on table kb_a.customers is 'People'; \
         create table kb_b.orders (id int primary key, customer_id int references kb_a.customers(id), total numeric(10,2)); \
         create view kb_b.big_orders as select * from kb_b.orders where total > 100; \
         create table kb_c.untouched (x int); insert into kb_c.untouched select generate_series(1, 100); analyze kb_c.untouched")
    .await;
    let m = s.bulk_metadata(&["kb_a".into(), "kb_b".into()]).await.unwrap();
    assert_eq!(m[0].objects.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["customers"]);
    assert_eq!(m[0].objects[0].comment.as_deref(), Some("People"));
    let cust = &m[0].columns[0];
    assert!(cust.columns[0].is_primary_key && cust.columns[1].comment.as_deref() == Some("login"));
    let orders = m[1].columns.iter().find(|t| t.table == "orders").unwrap();
    assert_eq!(orders.columns.iter().map(|c| c.data_type.as_str()).collect::<Vec<_>>(), vec!["integer", "integer", "numeric(10,2)"]);
    assert_eq!((orders.foreign_keys[0].ref_schema.as_str(), orders.foreign_keys[0].ref_table.as_str()), ("kb_a", "customers"));
    assert!(m[1].objects.iter().any(|o| o.name == "big_orders" && o.kind == ObjectKind::View));
    // Same result as the per-schema path.
    assert_eq!(s.schema_columns("kb_b").await.unwrap().iter().find(|t| t.table == "orders").unwrap(), orders);

    let fp = |m: &std::collections::HashMap<String, String>, k: &str| m.get(k).cloned().unwrap();
    let f0 = s.explorer_fingerprints().await.unwrap().unwrap();
    // This test's schemas only: other live tests change theirs meanwhile.
    let mine = |m: &std::collections::HashMap<String, String>| ["kb_a", "kb_b", "kb_c"].map(|k| fp(m, k));
    assert_eq!(mine(&f0), mine(&s.explorer_fingerprints().await.unwrap().unwrap()), "stable without changes");
    run("alter table kb_a.customers add column name text").await;
    let f1 = s.explorer_fingerprints().await.unwrap().unwrap();
    assert_ne!(fp(&f0, "kb_a"), fp(&f1, "kb_a"), "new column");
    assert_eq!(fp(&f0, "kb_c"), fp(&f1, "kb_c"), "other schemas unchanged");
    run("comment on column kb_b.orders.total is 'gross'").await;
    let f2 = s.explorer_fingerprints().await.unwrap().unwrap();
    assert_ne!(fp(&f1, "kb_b"), fp(&f2, "kb_b"), "comment");
    run("alter table kb_b.orders rename column total to amount").await;
    let f3 = s.explorer_fingerprints().await.unwrap().unwrap();
    assert_ne!(fp(&f2, "kb_b"), fp(&f3, "kb_b"), "renamed column");
    run("drop view kb_b.big_orders").await;
    assert_ne!(fp(&f3, "kb_b"), fp(&s.explorer_fingerprints().await.unwrap().unwrap(), "kb_b"), "dropped view");
    // 100 -> 101 rows: the estimate stays in its power of two (a table that doubles does change the value).
    run("insert into kb_c.untouched values (1); analyze kb_c.untouched").await;
    assert_eq!(fp(&f3, "kb_c"), fp(&s.explorer_fingerprints().await.unwrap().unwrap(), "kb_c"), "small data changes are not schema changes");
    run("drop schema kb_a cascade; drop schema kb_b cascade; drop schema kb_c cascade").await;
}

/// Runs against a live server when `DATABRAIN_PG_URL`-style env vars are set:
/// DATABRAIN_PG_HOST, DATABRAIN_PG_USER, DATABRAIN_PG_PASSWORD, DATABRAIN_PG_DB.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_roundtrip() {
    let Ok(host) = std::env::var("DATABRAIN_PG_HOST") else {
        panic!("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    };
    use databrain_auth::{AuthMethod, InlineCredentialSource};
    let user = std::env::var("DATABRAIN_PG_USER").unwrap_or_else(|_| "postgres".into());
    let pw = std::env::var("DATABRAIN_PG_PASSWORD").ok();
    let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: user.clone() });
    cfg.host = Some(host);
    cfg.port = std::env::var("DATABRAIN_PG_PORT").ok().and_then(|p| p.parse().ok());
    cfg.database = std::env::var("DATABRAIN_PG_DB").ok();
    let creds = Arc::new(InlineCredentialSource::new(AuthMethod::Password { user }, pw.map(secrecy::SecretString::from)));
    let s = PostgresConnector.connect(&cfg, creds).await.unwrap();
    let r = s
        .execute(
            databrain_connector_core::Statement::internal("select 1::int as a, 'x'::text as b, now() as c, 1.5::numeric as d, null::date as e"),
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(r.num_rows(), 1);
    let cancel = databrain_connector_core::CancellationToken::new();
    let stream = s
        .execute(
            databrain_connector_core::Statement::internal("select pg_sleep(30)"),
            ExecOptions { batch_size: 10, cancel: cancel.clone(), ..Default::default() },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();
    assert!(stream.collect().await.unwrap_err().is_cancelled());
    assert!(!s.list_schemas().await.unwrap().is_empty());
    // Indexes + partition key for query hints.
    let run = |sql: &'static str| s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default());
    run("drop table if exists databrain_layout_t").await.unwrap().collect().await.unwrap();
    run("create table databrain_layout_t (id int, day date not null, cust text, primary key (id, day)) partition by range (day)")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    run("create index databrain_layout_i on databrain_layout_t (cust, lower(cust))").await.unwrap().collect().await.unwrap();
    let l = s.table_layout("public", "databrain_layout_t").await.unwrap();
    run("drop table databrain_layout_t").await.unwrap().collect().await.unwrap();
    assert_eq!(l.partition_by, vec!["day"], "{l:?}");
    assert_eq!(l.partition_kind.as_deref(), Some("range"));
    assert!(l.indexes[0].primary && l.indexes[0].columns == vec!["id", "day"], "{l:?}");
    assert_eq!(l.indexes[1].columns, vec!["cust", "lower(cust)"], "{l:?}");
}

/// Live: the transaction state follows the server when a statement that
/// begins or ends one fails (its text said otherwise): a refused BEGIN or
/// `COMMIT AND CHAIN` leaves none open; a failed one inside a transaction
/// leaves it open (aborted, waiting for ROLLBACK).
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_transaction_state_after_a_failed_statement() {
    use databrain_connector_core::TxState;
    let Some(s) = live_session().await else {
        panic!("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    };
    let s = &s;
    let run = |sql: &'static str| async move {
        match s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default()).await {
            Ok(st) => st.collect().await.is_ok(),
            Err(_) => false,
        }
    };
    // Refused: no transaction began.
    assert!(!run("begin isolation level bogus").await);
    assert_eq!(s.transaction_state(), TxState::None, "a BEGIN that failed");
    assert!(!run("commit and chain").await);
    assert_eq!(s.transaction_state(), TxState::None, "COMMIT AND CHAIN outside a transaction");

    // A deferred constraint fails the COMMIT: the transaction is gone, no chain.
    for sql in [
        "drop table if exists kb_tx_child, kb_tx_parent",
        "create table kb_tx_parent (id int primary key)",
        "create table kb_tx_child (p int references kb_tx_parent(id) deferrable initially deferred)",
        "begin",
        "insert into kb_tx_child values (1)",
    ] {
        assert!(run(sql).await, "{sql}");
    }
    assert_eq!(s.transaction_state(), TxState::Open);
    assert!(!run("commit and chain").await);
    assert_eq!(s.transaction_state(), TxState::None, "a failed COMMIT AND CHAIN ended it");

    // Inside a transaction a failed statement leaves it open (aborted).
    assert!(run("begin").await);
    assert!(!run("rollback to savepoint nosuch").await);
    assert_eq!(s.transaction_state(), TxState::Open, "still open, aborted");
    assert!(!run("select 1").await, "aborted: commands are refused");
    assert!(run("rollback").await);
    assert_eq!(s.transaction_state(), TxState::None);
    assert!(run("drop table kb_tx_child, kb_tx_parent").await);
}

/// Live: inside a transaction the row limit fetches only the rows it keeps
/// (through a cursor) instead of reading the result to its end; the
/// transaction stays usable; a read a cursor can't hold (a CTE that writes)
/// runs as before, and the transaction isn't aborted by trying.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_row_limit_in_a_transaction_reads_only_its_rows() {
    let Some(s) = live_session().await else {
        panic!("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    };
    let s = &s;
    let run = |sql: &'static str, max: Option<usize>| async move {
        let opts = ExecOptions { max_rows: max, ..Default::default() };
        let r = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            s.execute(databrain_connector_core::Statement::internal(sql), opts).await.unwrap().collect().await
        })
        .await
        .unwrap_or_else(|_| panic!("{sql}: no answer in 20 s"));
        r.unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    let rows = |c: &databrain_connector_core::Collected| c.batches.iter().map(|b| b.num_rows()).sum::<usize>();
    run("drop table if exists kb_rl", None).await;
    run("create table kb_rl (id int)", None).await;
    run("begin", None).await;
    run("insert into kb_rl values (1)", None).await;
    // 20 000 rows at 1 ms each: about 20 s when read to the end.
    let t = std::time::Instant::now();
    let c = run("select g, pg_sleep(0.001) from generate_series(1, 20000) g", Some(10)).await;
    assert_eq!(rows(&c), 10);
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "read to the end: {:?}", t.elapsed());
    // Still in the transaction, and it still works.
    assert_eq!(s.transaction_state(), databrain_connector_core::TxState::Open);
    assert_eq!(rows(&run("select count(*) from kb_rl", Some(10)).await), 1);
    // A CTE that writes can't be a cursor: it runs as before, nothing aborted.
    assert_eq!(rows(&run("with w as (insert into kb_rl values (2) returning id) select id from w", Some(10)).await), 1);
    run("commit", None).await;
    let c = run("select count(*)::text from kb_rl", None).await;
    let n = databrain_connector_core::arrow::util::display::array_value_to_string(c.batches[0].column(0), 0).unwrap();
    assert_eq!(n, "2", "both inserts committed");
    run("drop table kb_rl", None).await;
}

/// Live: Stop while rows are coming stops the query on the server: the
/// next statement on the session answers at once (it waited for the rest
/// of the result when Stop came while a row was ready).
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_stop_while_rows_arrive_cancels_on_the_server() {
    let Some(s) = live_session().await else {
        panic!("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    };
    let cancel = databrain_connector_core::CancellationToken::new();
    let opts = ExecOptions { cancel: cancel.clone(), batch_size: 10, ..Default::default() };
    // Rows as fast as the server sends them (a row is ready whenever Stop
    // comes), for longer than the test waits.
    let mut st = s.execute(databrain_connector_core::Statement::internal("select g from generate_series(1, 100000000) g"), opts).await.unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(ev) = st.next().await {
            if matches!(ev, Ok(databrain_connector_core::StreamEvent::Batch(_))) {
                return true;
            }
        }
        false
    })
    .await
    .expect("a first batch");
    assert!(first);
    cancel.cancel();
    let err = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let mut err = None;
        while let Some(ev) = st.next().await {
            if let Err(e) = ev {
                err = Some(e);
            }
        }
        err
    })
    .await
    .expect("the stream ends");
    assert!(err.is_some_and(|e| e.is_cancelled()), "cancelled");
    let t = std::time::Instant::now();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        s.execute(databrain_connector_core::Statement::internal("select 1"), ExecOptions::default()).await.unwrap().collect().await.unwrap()
    })
    .await
    .expect("the session answers");
    assert!(t.elapsed() < std::time::Duration::from_secs(3), "waited {:?} for the rest of the result", t.elapsed());
}

/// The row limit's cursor is used inside a transaction, never on Redshift
/// (no SAVEPOINT there: the failed one would abort the user's transaction).
#[test]
fn the_row_limit_cursor_is_off_on_redshift() {
    use crate::exec::cursor_for_limit;
    assert!(cursor_for_limit(false, true));
    assert!(!cursor_for_limit(false, false), "outside a transaction: not needed");
    assert!(!cursor_for_limit(true, true), "Redshift: refused");
}

/// Live: the signed-in user changes its own password (hashed here, as
/// psql's \password): the new one signs in, the old one no longer does.
#[ignore = "live: needs DATABRAIN_PG_HOST (cargo test -- --ignored)"]
#[tokio::test]
async fn live_change_password() {
    let Some(admin) = live_session().await else {
        panic!("live test: DATABRAIN_PG_HOST not set (run with --ignored)");
    };
    let as_user = |pw: &'static str| async move {
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: "kb_pw".into() });
        cfg.host = std::env::var("DATABRAIN_PG_HOST").ok();
        cfg.port = std::env::var("DATABRAIN_PG_PORT").ok().and_then(|p| p.parse().ok());
        cfg.database = Some(std::env::var("DATABRAIN_PG_DB").unwrap_or_else(|_| "postgres".into()));
        PostgresConnector
            .connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::Password { user: "kb_pw".into() }, Some(secrecy::SecretString::from(pw)))))
            .await
    };
    let _ = admin.run_internal("drop role if exists kb_pw").await;
    admin.run_internal("create role kb_pw login password 'Old_pw_1'").await.unwrap();
    let s = as_user("Old_pw_1").await.unwrap();
    s.change_password("Old_pw_1", "New'pw\\2").await.unwrap();
    drop(s);
    assert!(as_user("New'pw\\2").await.is_ok(), "the new password signs in");
    assert!(as_user("Old_pw_1").await.is_err(), "the old one no longer does");
    admin.run_internal("drop role kb_pw").await.unwrap();
}
