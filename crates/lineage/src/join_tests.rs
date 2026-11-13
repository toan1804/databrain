use super::*;
use databrain_connector_core::ConnectorKind as K;

fn cat(parts: &[String]) -> Option<CatalogTable> {
    if parts.len() > 1 && !parts[parts.len() - 2].eq_ignore_ascii_case("sales") {
        return None;
    }
    let name = parts.last()?.to_lowercase();
    let (cols, view_sql): (&[&str], Option<&str>) = match name.as_str() {
        "orders" => (&["id", "customer_id", "amount", "status", "line_no"], None),
        "customers" => (&["id", "name", "country"], None),
        "order_items" => (&["order_id", "line_no", "product_id", "qty"], None),
        "shipments" => (&["order_id", "shipped_at"], None),
        "products" => (&["id", "name"], None),
        "employees" => (&["id", "manager_id", "dept"], None),
        "paid_orders" => (
            &["id", "customer_id"],
            Some("create view paid_orders as select o.id, o.customer_id from orders o join customers c on c.id = o.customer_id where o.status = 'paid'"),
        ),
        _ => return None,
    };
    Some(CatalogTable { name: format!("sales.{name}"), columns: cols.iter().map(|c| c.to_string()).collect(), view_sql: view_sql.map(str::to_string) })
}

/// `sales.orders(customer_id) = sales.customers(id) [inner]` lines.
fn joins_in(sql: &str, kind: K) -> Vec<String> {
    join_pairs(sql, kind, &cat)
        .into_iter()
        .map(|j| format!("{}({}) = {}({}) [{}]", j.left, j.left_columns.join(", "), j.right, j.right_columns.join(", "), j.kind.as_str()))
        .collect()
}

fn pg(sql: &str) -> Vec<String> {
    joins_in(sql, K::Postgres)
}

#[test]
fn on_equality_is_learned_in_written_order() {
    assert_eq!(pg("select * from orders o join customers c on c.id = o.customer_id"), ["sales.orders(customer_id) = sales.customers(id) [inner]"]);
}

#[test]
fn outer_join_kinds() {
    assert_eq!(pg("select * from orders o left join customers c on c.id = o.customer_id"), ["sales.orders(customer_id) = sales.customers(id) [left]"]);
    assert_eq!(pg("select * from orders o right outer join customers c on o.customer_id = c.id"), ["sales.orders(customer_id) = sales.customers(id) [right]"]);
    assert_eq!(pg("select * from orders o full join customers c on o.customer_id = c.id"), ["sales.orders(customer_id) = sales.customers(id) [full]"]);
    assert_eq!(JoinKind::Left.flipped(), JoinKind::Right);
    assert_eq!(JoinKind::Inner.flipped(), JoinKind::Inner);
}

#[test]
fn composite_key_is_one_pair_and_literals_are_skipped() {
    assert_eq!(
        pg("select * from orders o join order_items i on i.order_id = o.id and (i.line_no = o.line_no) and i.qty > 0 and o.status = 'paid'"),
        ["sales.orders(id, line_no) = sales.order_items(order_id, line_no) [inner]"]
    );
}

#[test]
fn using_columns() {
    assert_eq!(pg("select * from order_items i join shipments s using (order_id)"), ["sales.order_items(order_id) = sales.shipments(order_id) [inner]"]);
}

#[test]
fn several_joins_in_one_query() {
    assert_eq!(
        pg(
            "select * from orders o join order_items i on i.order_id = o.id join products p on p.id = i.product_id left join customers c on c.id = o.customer_id"
        ),
        [
            "sales.orders(id) = sales.order_items(order_id) [inner]",
            "sales.order_items(product_id) = sales.products(id) [inner]",
            "sales.orders(customer_id) = sales.customers(id) [left]",
        ]
    );
}

#[test]
fn implicit_join_in_where() {
    assert_eq!(
        pg("select * from orders o, customers c where o.customer_id = c.id and o.status = 'paid'"),
        ["sales.orders(customer_id) = sales.customers(id) [where]"]
    );
}

#[test]
fn correlated_subquery_links_to_outer_query() {
    assert_eq!(
        pg("select * from customers c where exists (select 1 from orders o where o.customer_id = c.id)"),
        ["sales.orders(customer_id) = sales.customers(id) [where]"]
    );
}

#[test]
fn cte_and_subquery_columns_passed_on_unchanged_are_traced() {
    assert_eq!(
        pg("with x as (select customer_id, amount from orders) select * from x join customers c on c.id = x.customer_id"),
        ["sales.orders(customer_id) = sales.customers(id) [inner]"]
    );
    assert_eq!(
        pg("select * from (select o.customer_id as cid from orders o) s join customers c on c.id = s.cid"),
        ["sales.orders(customer_id) = sales.customers(id) [inner]"]
    );
}

#[test]
fn computed_or_combined_columns_are_not_learned() {
    // Computed in a CTE.
    assert!(pg("with x as (select customer_id + 1 as k from orders) select * from x join customers c on c.id = x.k").is_empty());
    // Aggregated.
    assert!(pg("with x as (select max(customer_id) as k from orders) select * from x join customers c on c.id = x.k").is_empty());
    // A union: two origins.
    assert!(pg("with x as (select customer_id as k from orders union all select id from products) select * from x join customers c on c.id = x.k").is_empty());
    // Functions in the condition.
    assert!(pg("select * from orders o join customers c on lower(c.name) = lower(o.status)").is_empty());
}

#[test]
fn casts_are_seen_through() {
    assert_eq!(
        pg("select * from orders o join customers c on c.id::text = cast(o.customer_id as text)"),
        ["sales.orders(customer_id) = sales.customers(id) [inner]"]
    );
}

#[test]
fn or_and_non_equi_conditions_are_not_learned() {
    assert!(pg("select * from orders o join customers c on c.id = o.customer_id or c.id = o.id").is_empty());
    assert!(pg("select * from orders o join customers c on o.amount > c.id").is_empty());
    assert!(pg("select * from orders o join customers c on c.id = 5").is_empty());
}

#[test]
fn a_composite_key_with_a_part_that_cannot_be_placed_is_dropped_whole() {
    assert!(
        pg("with x as (select order_id, line_no + 1 as ln from order_items) select * from orders o join x on x.order_id = o.id and x.ln = o.line_no")
            .is_empty()
    );
}

#[test]
fn self_joins() {
    assert_eq!(pg("select * from employees e join employees m on m.id = e.manager_id"), ["sales.employees(manager_id) = sales.employees(id) [inner]"]);
    // The same table on its own key says nothing.
    assert!(pg("select * from orders o where exists (select 1 from orders o2 where o2.id = o.id)").is_empty());
    // One source compared with itself is a filter.
    assert!(pg("select * from employees e join customers c on e.id = e.manager_id").is_empty());
}

#[test]
fn ambiguous_or_unknown_columns_are_not_learned() {
    // `id` is in both tables.
    assert!(pg("select * from orders join customers on id = customer_id").is_empty());
    // No catalog: unqualified columns can't be placed.
    assert!(join_pairs("select * from t1 join t2 on k = j", K::Postgres, &NoCatalog).is_empty());
    // Unknown alias.
    assert!(pg("select * from orders o join customers c on c.id = zz.customer_id").is_empty());
}

#[test]
fn qualified_columns_of_tables_the_catalog_does_not_know() {
    let j = join_pairs("select * from crm.accounts a join billing.invoices i on i.account_ref = a.acct_no", K::Postgres, &NoCatalog);
    assert_eq!(j.len(), 1);
    assert_eq!((j[0].left.as_str(), j[0].left_columns.as_slice()), ("crm.accounts", &["acct_no".to_string()][..]));
    assert_eq!((j[0].right.as_str(), j[0].right_columns.as_slice()), ("billing.invoices", &["account_ref".to_string()][..]));
}

#[test]
fn tables_the_script_creates_are_not_learned() {
    assert!(pg("create temp table tmp as select customer_id from orders; select * from tmp join customers c on c.id = tmp.customer_id").is_empty());
}

#[test]
fn views_are_learned_as_views_and_their_definition_is_not() {
    assert_eq!(pg("select * from paid_orders p join order_items i on i.order_id = p.id"), ["sales.paid_orders(id) = sales.order_items(order_id) [inner]"]);
}

#[test]
fn update_delete_and_merge() {
    assert_eq!(
        pg("update orders o set amount = 0 from customers c where c.id = o.customer_id and c.country = 'x'"),
        ["sales.orders(customer_id) = sales.customers(id) [where]"]
    );
    assert_eq!(pg("delete from orders o using customers c where c.id = o.customer_id"), ["sales.orders(customer_id) = sales.customers(id) [where]"]);
    assert_eq!(
        pg("merge into shipments s using order_items i on s.order_id = i.order_id when matched then update set shipped_at = now()"),
        ["sales.shipments(order_id) = sales.order_items(order_id) [inner]"]
    );
}

#[test]
fn statements_of_a_script_are_numbered_and_duplicates_kept_once() {
    let j = join_pairs(
        "select 1; select * from orders o join customers c on c.id = o.customer_id; select * from orders o join customers c on c.id = o.customer_id",
        K::Postgres,
        &cat,
    );
    assert_eq!(j.len(), 2);
    assert_eq!((j[0].statement, j[1].statement), (1, 2));
}

#[test]
fn unparsable_sql_gives_nothing() {
    assert!(pg("select * from orders o join customers c on c.id = ").is_empty());
}

#[test]
fn dialect_quoting() {
    let want = ["sales.orders(customer_id) = sales.customers(id) [inner]"];
    assert_eq!(joins_in(r#"select * from "orders" o join "customers" c on c."id" = o."customer_id""#, K::Postgres), want);
    assert_eq!(joins_in("select * from `orders` o join `customers` c on c.`id` = o.`customer_id`", K::Mysql), want);
    assert_eq!(joins_in("select * from [sales].[orders] o join [customers] c on c.[id] = o.[customer_id]", K::Mssql), want);
    assert_eq!(joins_in(r#"SELECT * FROM ORDERS O JOIN "CUSTOMERS" C ON C.ID = O.CUSTOMER_ID"#, K::Oracle), want);
    assert_eq!(joins_in("select * from `sales.orders` o join sales.customers c on c.id = o.customer_id", K::Bigquery), want);
    assert_eq!(joins_in("select * from sales.orders o join sales.customers c on c.id = o.customer_id", K::Duckdb), want);
    // ASOF: the equality is the key, the match condition is not.
    assert_eq!(joins_in("select * from orders o asof join customers c match_condition (o.id >= c.id) on c.id = o.customer_id", K::Snowflake), want);
}

#[test]
fn fingerprint_ignores_values_spacing_comments_and_case() {
    let a = fingerprint("select * from orders where id = 5 and status = 'paid'", K::Postgres);
    assert_eq!(a, fingerprint("SELECT *\n  FROM orders -- recent\n WHERE id = 42 AND status = 'open';", K::Postgres));
    assert_eq!(a.len(), 16);
    assert_eq!(fingerprint("select * from t where id in (1, 2, 3)", K::Postgres), fingerprint("select * from t where id in (7)", K::Postgres));
    assert_eq!(fingerprint("select * from t where id = $1", K::Postgres), fingerprint("select * from t where id = 9", K::Postgres));
    // Other tables, columns or quoted names are other queries.
    assert_ne!(a, fingerprint("select * from customers where id = 5 and status = 'paid'", K::Postgres));
    assert_ne!(fingerprint(r#"select "Id" from t"#, K::Postgres), fingerprint("select id from t", K::Postgres));
    // Stable value (FNV-1a): must not change between releases.
    assert_eq!(fingerprint("select 1", K::Postgres), fingerprint("SELECT 2", K::Postgres));
    // A sign belongs to its number; a minus after a value subtracts.
    assert_eq!(fingerprint("select * from t where a = -1", K::Postgres), fingerprint("select * from t where a = 1", K::Postgres));
    assert_eq!(fingerprint("select * from t where a in (-1, 2)", K::Postgres), fingerprint("select * from t where a in (3)", K::Postgres));
    assert_ne!(fingerprint("select a - 1 from t", K::Postgres), fingerprint("select a from t", K::Postgres));
    assert_ne!(fingerprint("select a - 1 from t", K::Postgres), fingerprint("select a 1 from t", K::Postgres));
    // Any number of rows of literals.
    assert_eq!(fingerprint("insert into t values (1, 'a'), (2, 'b'), (3, 'c')", K::Postgres), fingerprint("insert into t values (4, 'd')", K::Postgres));
    assert_ne!(fingerprint("insert into t values (1), (now())", K::Postgres), fingerprint("insert into t values (1)", K::Postgres));
}
