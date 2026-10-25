use super::*;
use ConnectorKind as K;

fn cat(parts: &[String]) -> Option<CatalogTable> {
    // Like the explorer cache: a schema, when given, must match.
    if parts.len() > 1 && !parts[parts.len() - 2].eq_ignore_ascii_case("sales") {
        return None;
    }
    let name = parts.last()?.to_lowercase();
    let cols: &[&str] = match name.as_str() {
        "orders" => &["id", "customer_id", "amount", "status", "created_at"],
        "customers" => &["id", "name", "country"],
        _ => return None,
    };
    Some(CatalogTable { name: format!("sales.{name}"), columns: cols.iter().map(|c| c.to_string()).collect() })
}

fn run(sql: &str) -> Lineage {
    analyze(sql, K::Postgres, &cat)
}

fn name(g: &Lineage, id: &str) -> String {
    g.nodes.iter().find(|n| n.id == id).unwrap().name.clone()
}

/// `orders.amount -aggregate-> Result.total` style lines, sorted.
fn edges(g: &Lineage) -> Vec<String> {
    let mut v: Vec<String> = g
        .edges
        .iter()
        .map(|e| {
            let k = serde_json_kind(e.kind);
            format!(
                "{}{} -{k}-> {}{}",
                name(g, &e.from),
                e.from_column.as_ref().map(|c| format!(".{c}")).unwrap_or_default(),
                name(g, &e.to),
                e.to_column.as_ref().map(|c| format!(".{c}")).unwrap_or_default()
            )
        })
        .collect();
    v.sort();
    v
}

fn serde_json_kind(k: EdgeKind) -> &'static str {
    match k {
        EdgeKind::Direct => "direct",
        EdgeKind::Transform => "transform",
        EdgeKind::Aggregate => "aggregate",
        EdgeKind::Filter => "filter",
        EdgeKind::Join => "join",
    }
}

#[test]
fn select_join_aggregate_filter() {
    let g = run(
        "select c.name, sum(o.amount) as total, upper(c.country) country_u \
         from orders o join customers c on c.id = o.customer_id \
         where o.status = 'paid' group by 1, 3",
    );
    assert_eq!(
        edges(&g),
        vec![
            "sales.customers.country -transform-> Result.country_u",
            "sales.customers.id -join-> Result",
            "sales.customers.name -direct-> Result.name",
            "sales.orders.amount -aggregate-> Result.total",
            "sales.orders.customer_id -join-> Result",
            "sales.orders.status -filter-> Result",
        ]
    );
    assert!(g.warnings.is_empty(), "{:?}", g.warnings);
    assert_eq!(g.statements[0].kind, "select");
    let detail = |c: &str| g.edges.iter().find(|e| e.from_column.as_deref() == Some(c) && e.to_column.is_none()).and_then(|e| e.detail.clone());
    assert_eq!(detail("customer_id").as_deref(), Some("JOIN customers c ON c.id = o.customer_id"));
    assert_eq!(detail("status").as_deref(), Some("WHERE o.status = 'paid'"));
    let g2 = run("select o.id from orders o left join customers c using (id)");
    assert!(g2.edges.iter().all(|e| e.kind != EdgeKind::Join || e.detail.as_deref() == Some("LEFT JOIN customers c USING (id)")), "{:?}", g2.edges);
    let total = g.nodes.iter().find(|n| n.name == "Result").unwrap().columns.iter().find(|c| c.name == "total").unwrap();
    assert_eq!(total.expr.as_deref(), Some("sum(o.amount)"));
}

#[test]
fn ctes_star_and_subqueries() {
    let g = run(
        "with paid as (select * from orders where status = 'paid'), \
              per_c as (select customer_id, count(*) n from paid group by customer_id) \
         select p.customer_id, p.n, (select max(amount) from orders) as top from per_c p",
    );
    let e = edges(&g);
    assert!(e.contains(&"paid.customer_id -direct-> per_c.customer_id".to_string()), "{e:#?}");
    assert!(e.contains(&"sales.orders.amount -direct-> paid.amount".to_string()), "star expanded from the catalog");
    assert!(e.contains(&"sales.orders.status -filter-> paid".to_string()));
    assert!(e.contains(&"per_c.n -direct-> Result.n".to_string()));
    assert!(e.contains(&"sales.orders.amount -aggregate-> SELECT.max(amount)".to_string()), "{e:#?}");
    assert!(e.contains(&"SELECT.max(amount) -transform-> Result.top".to_string()), "{e:#?}");
    let paid = g.nodes.iter().find(|n| n.name == "paid").unwrap();
    assert_eq!(paid.kind, NodeKind::Cte);
    assert_eq!(paid.columns.len(), 5);
}

#[test]
fn union_by_position_and_unknown_tables() {
    let g = run("select id, name from customers union all select id, label from archive.people");
    let e = edges(&g);
    assert!(e.contains(&"archive.people.label -direct-> SELECT.label".to_string()), "{e:#?}");
    let res = g.nodes.iter().find(|n| n.kind == NodeKind::Result).unwrap();
    assert_eq!(res.name, "Result");
    assert_eq!(res.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "name"]);
    assert!(e.iter().any(|x| x.ends_with("-direct-> Result.name") && x.starts_with("SELECT.label")), "{e:#?}");
    assert_eq!(g.unknown_tables, vec!["archive.people"]);
    // Star on an uncached table: one edge for the whole table, and a warning.
    let g = run("select * from archive.people");
    assert_eq!(edges(&g), vec!["archive.people -direct-> Result.people.*"]);
    assert!(g.warnings[0].message.contains("not cached"));
}

#[test]
fn script_links_written_tables() {
    let g = run(
        "create table tmp_paid as select id, amount from orders where status = 'paid';\n\
         insert into mart_total (total) select sum(amount) from tmp_paid;\n\
         select * from mart_total",
    );
    let e = edges(&g);
    assert!(e.contains(&"sales.orders.amount -direct-> SELECT.amount".to_string()), "{e:#?}");
    assert!(e.contains(&"SELECT.amount -direct-> tmp_paid.amount".to_string()));
    assert!(e.contains(&"tmp_paid.amount -aggregate-> SELECT.sum(amount)".to_string()), "same node read by the next statement: {e:#?}");
    assert!(e.contains(&"SELECT.sum(amount) -direct-> mart_total.total".to_string()));
    // INSERT doesn't tell the table's other columns: `*` stays one edge.
    assert!(e.contains(&"mart_total -direct-> Result 3.mart_total.*".to_string()), "{e:#?}");
    // CREATE TABLE AS does: `*` expands.
    let g2 = run("create table t2 as select id, amount from orders; select * from t2");
    assert!(edges(&g2).contains(&"t2.amount -direct-> Result 2.amount".to_string()), "{:#?}", edges(&g2));
    let kinds: Vec<&str> = g.statements.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, vec!["create_table_as", "insert", "select"]);
    assert!(g.nodes.iter().find(|n| n.name == "tmp_paid").unwrap().written);
    assert_eq!(g.unknown_tables, vec!["mart_total"], "tables the script creates are not unknown; one it inserts into is");
}

#[test]
fn warnings_not_guesses() {
    let g = run("select nope, o.id from orders o join customers c on true");
    assert!(g.warnings.iter().any(|w| w.message == "Unknown column nope"), "{:?}", g.warnings);
    let g = run("select id from orders o join customers c on c.id = o.customer_id");
    assert!(g.warnings.iter().any(|w| w.message.contains("ambiguous")), "{:?}", g.warnings);
    // Two uncached tables: an unqualified column can't be assigned.
    let g = run("select x from a join b on a.k = b.k");
    assert!(g.warnings.iter().any(|w| w.message.contains("Can't tell which table x")), "{:?}", g.warnings);
}

#[test]
fn spans_point_at_the_sql() {
    let sql = "select 1;\nselect o.amount as amt from orders o";
    let g = run(sql);
    let res = g.nodes.iter().find(|n| n.name == "Result 2").unwrap();
    let r = res.columns[0].span.unwrap();
    assert_eq!(&sql[r.start..r.end], "amt");
    let e = g.edges.iter().find(|e| e.to_column.as_deref() == Some("amt")).unwrap();
    let r = e.span.unwrap();
    assert_eq!(&sql[r.start..r.end], "o.amount");
    assert_eq!(&sql[g.statements[1].range.start..g.statements[1].range.end], "select o.amount as amt from orders o");
    // Multi-byte text before the span.
    let sql = "select 'é😀' as x, o.status from orders o";
    let g = run(sql);
    let e = g.edges.iter().find(|e| e.to_column.as_deref() == Some("status")).unwrap();
    let r = e.span.unwrap();
    assert_eq!(&sql[r.start..r.end], "o.status");
}

#[test]
fn recursive_cte_and_dialects() {
    let g = run("with recursive t(n) as (select 1 union all select n + 1 from t where n < 5) select n from t");
    let e = edges(&g);
    assert!(e.contains(&"t.n -direct-> Result.n".to_string()), "{e:#?}");
    assert!(g.nodes.iter().filter(|n| n.name == "t").count() == 1, "placeholder merged");
    let g = analyze("select top 5 [o].[amount] from [sales].[orders] as [o]", K::Mssql, &cat);
    assert_eq!(edges(&g), vec!["sales.orders.amount -direct-> Result.amount"]);
    let g = analyze("select `o`.amount from orders `o`", K::Mysql, &cat);
    assert_eq!(edges(&g), vec!["sales.orders.amount -direct-> Result.amount"]);
}

#[test]
fn unparsable_statement_is_partial() {
    let g = run("select from from orders where (((;\nselect id from customers");
    assert!(g.partial);
    assert_eq!(g.statements[0].kind, "error");
    assert!(g.statements[0].error.is_some());
    assert!(g.nodes.iter().any(|n| n.name == "sales.orders"), "tables found by tokens");
    assert_eq!(edges(&g), vec!["sales.customers.id -direct-> Result 2.id"]);
}

/// `from -kind-> to [statements]` lines of the table-level chain, sorted.
fn flows_of(g: &Lineage) -> Vec<String> {
    let mut v: Vec<String> = g
        .flows
        .iter()
        .map(|f| format!("{} -{}-> {} {:?}", name(g, &f.from), serde_json_kind(f.kind), name(g, &f.to), f.statements))
        .collect();
    v.sort();
    v
}

#[test]
fn chain_through_insert_update_ctas_merge_delete() {
    let g = run(
        "insert into a (id, amount) select o.id, o.amount from (select * from orders where status = 'paid') o join customers c on c.id = o.customer_id;\n\
         update b set total = a.amount * 2 from a where b.id = a.id;\n\
         create table c as select * from b;\n\
         merge into d using c on d.id = c.id when matched then update set total = c.total when not matched then insert (id, total) values (c.id, c.total);\n\
         delete from d using customers x where d.id = x.id and x.country = 'VN';\n\
         select * from d",
    );
    let kinds: Vec<&str> = g.statements.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, vec!["insert", "update", "create_table_as", "merge", "delete", "select"]);
    assert_eq!(
        flows_of(&g),
        vec![
            "a -transform-> b [1]",
            "b -direct-> c [2]",
            "c -direct-> d [3]",
            "d -direct-> Result 6 [5]",
            "sales.customers -filter-> d [4]",
            "sales.customers -join-> a [0]",
            "sales.orders -direct-> a [0]",
        ],
        "the whole chain, table to table"
    );
    let e = edges(&g);
    assert!(e.contains(&"a.amount -transform-> b.total".to_string()), "{e:#?}");
    assert!(e.contains(&"a.id -filter-> b".to_string()), "UPDATE … WHERE: {e:#?}");
    assert!(e.contains(&"c.total -direct-> d.total".to_string()) && e.contains(&"c.id -direct-> d.id".to_string()), "{e:#?}");
    assert!(e.contains(&"c.id -join-> d".to_string()), "MERGE … ON: {e:#?}");
    assert!(e.contains(&"sales.customers.country -filter-> d".to_string()), "{e:#?}");
    assert!(!g.edges.iter().any(|e| e.from == e.to), "no self edges (b.id in WHERE, d.id in ON)");
    for t in ["a", "b", "c", "d"] {
        assert!(g.nodes.iter().find(|n| n.name == t).unwrap().written, "{t} written");
    }
    assert!(g.warnings.iter().all(|w| !w.message.contains("not traced yet")), "{:?}", g.warnings);
    // Every edge says which statement it comes from.
    let upd = g.edges.iter().find(|e| e.to_column.as_deref() == Some("total") && name(&g, &e.to) == "b").unwrap();
    assert_eq!(upd.statement, Some(1));
}

#[test]
fn chain_names_ddl_and_temp_tables() {
    // Written unqualified, read qualified (and the other way round): one node.
    let g = run("create table tmp as select id, amount from orders;\nselect amount from public.tmp;\ninsert into stage.t2 select * from tmp;\nselect * from t2");
    assert_eq!(g.nodes.iter().filter(|n| n.name.ends_with("tmp")).count(), 1, "{:?}", g.nodes.iter().map(|n| &n.name).collect::<Vec<_>>());
    assert!(flows_of(&g).contains(&"public.tmp -direct-> stage.t2 [2]".to_string()), "{:#?}", flows_of(&g));
    assert!(flows_of(&g).contains(&"stage.t2 -direct-> Result 4 [3]".to_string()), "{:#?}", flows_of(&g));
    // CREATE TABLE (cols) then INSERT without a column list: by position.
    let g = run("create temp table s (k int, v text);\ninsert into s select id, name from customers;\nselect v from s");
    let s = g.nodes.iter().find(|n| n.name == "s").unwrap();
    assert!(s.temporary && s.written && s.columns_known);
    let e = edges(&g);
    assert!(e.contains(&"SELECT.name -direct-> s.v".to_string()), "{e:#?}");
    assert!(e.contains(&"s.v -direct-> Result 3.v".to_string()), "{e:#?}");
    assert_eq!(g.statements[0].kind, "create_table");
    // A table recreated with other columns: the query's, not the catalog's.
    let g = run("drop table orders;\ncreate table orders as select 1 as x;\nselect * from orders");
    let e = edges(&g);
    assert!(e.contains(&"sales.orders.x -direct-> Result 3.x".to_string()), "{e:#?}");
    assert!(!e.iter().any(|x| x.contains("customer_id")), "{e:#?}");
    // SET a = a + 1 isn't a self loop; TRUNCATE / ALTER are known statements.
    let g = run("update orders set amount = amount + 1 where status = 'x';\ntruncate table customers;\nalter table customers add column vip boolean");
    assert!(g.edges.is_empty(), "{:?}", edges(&g));
    assert!(g.nodes.iter().find(|n| n.name == "sales.customers").unwrap().columns.iter().any(|c| c.name == "vip"));
    let kinds: Vec<&str> = g.statements.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, vec!["update", "truncate", "alter"]);
    assert!(g.warnings.is_empty(), "{:?}", g.warnings);
}

#[test]
fn select_into_starts_a_chain() {
    let g = analyze("select id, amount into #stage from orders where status = 'paid';\ninsert into mart (id, total) select id, amount from #stage", K::Mssql, &cat);
    assert_eq!(g.statements[0].kind, "select_into");
    let f = flows_of(&g);
    assert!(f.contains(&"#stage -direct-> mart [1]".to_string()), "{f:#?}");
    assert!(f.contains(&"sales.orders -direct-> #stage [0]".to_string()), "{f:#?}");
    assert!(!f.contains(&"sales.orders -filter-> #stage [0]".to_string()), "values beat the filter for the same pair: {f:#?}");
}

#[test]
fn chain_review_cases() {
    // The catalog's `orders` is sales.orders, not a written archive.orders.
    let g = run("insert into archive.orders (id) select id from customers;\nselect * from orders");
    assert!(g.nodes.iter().any(|n| n.name == "sales.orders") && g.nodes.iter().any(|n| n.name == "archive.orders"));
    assert!(edges(&g).contains(&"sales.orders.amount -direct-> Result 2.amount".to_string()), "{:#?}", edges(&g));
    // Two explicit schemas stay two tables; the first qualified label stays.
    let g = run("create table public.tmp as select id from orders;\nselect id from tmp;\nselect id from other.tmp");
    let names: Vec<&str> = g.nodes.iter().filter(|n| n.kind == NodeKind::Table).map(|n| n.name.as_str()).collect();
    assert!(names.contains(&"public.tmp") && names.contains(&"other.tmp"), "{names:?}");
    assert!(flows_of(&g).contains(&"public.tmp -direct-> Result 2 [1]".to_string()), "{:#?}", flows_of(&g));
    // MySQL multi-table DELETE / UPDATE … JOIN.
    let g = analyze("delete t1 from orders t1 join customers c on c.id = t1.customer_id where c.country = 'VN'", K::Mysql, &cat);
    assert!(!g.nodes.iter().any(|n| n.name == "t1"), "no table named after the alias");
    assert!(g.nodes.iter().find(|n| n.name == "sales.orders").unwrap().written);
    assert!(g.unknown_tables.is_empty(), "{:?}", g.unknown_tables);
    let g = analyze("update orders o join customers c on c.id = o.customer_id set c.name = o.status", K::Mysql, &cat);
    assert!(edges(&g).contains(&"sales.orders.status -direct-> sales.customers.name".to_string()), "{:#?}", edges(&g));
    assert!(g.nodes.iter().find(|n| n.name == "sales.customers").unwrap().written);
    // A recreated table has only its new columns.
    let g = run("select amount from orders;\ncreate table orders as select 1 as x;\nselect * from orders");
    let r3 = g.nodes.iter().find(|n| n.name == "Result 3").unwrap();
    assert_eq!(r3.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["x"]);
    // IF NOT EXISTS on a table that exists: its columns stay.
    let g = run("create table if not exists customers (k int);\nselect * from customers");
    let r2 = g.nodes.iter().find(|n| n.name == "Result 2").unwrap();
    assert_eq!(r2.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "name", "country"]);
    // A table only joined in a subquery decides rows: a join flow, not a value one.
    let g = run("select s.id from (select o.id, c.name from orders o join customers c on c.id = o.customer_id) s");
    assert_eq!(flows_of(&g), vec!["sales.customers -join-> Result [0]", "sales.orders -direct-> Result [0]"]);
    // Dropped only: not drawn, not looked up.
    let g = run("drop table if exists old_stuff");
    assert!(g.nodes.is_empty() && g.unknown_tables.is_empty());
}
