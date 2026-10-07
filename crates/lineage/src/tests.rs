use super::*;
use ConnectorKind as K;

fn cat(parts: &[String]) -> Option<CatalogTable> {
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
    assert!(g.unknown_tables.is_empty(), "written tables are not unknown: {:?}", g.unknown_tables);
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
