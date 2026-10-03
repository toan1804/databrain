import { describe, expect, it } from "vitest";
import { analyzeStatement, columnRoles, columnUses, layoutSummary, refsWithPos, type LayoutRef } from "./queryHints";
import type { TableLayout } from "./types";

const layout = (p: Partial<TableLayout>): TableLayout => ({ indexes: [], partition_by: [], cluster_by: [], requires_partition_filter: false, notes: [], ...p });

function hints(sql: string, layouts: Record<string, TableLayout>, kind?: "bigquery") {
  const tables: LayoutRef[] = refsWithPos(sql)
    .filter((r) => layouts[r.parts[r.parts.length - 1]])
    .map((ref) => ({ ref, layout: layouts[ref.parts[ref.parts.length - 1]] }));
  return analyzeStatement(sql, tables, kind).map((h) => ({ ...h, text: sql.slice(h.from, h.to) }));
}

const events = layout({ partition_by: ["event_date"], partition_kind: "day", cluster_by: ["user_id"] });
const orders = layout({
  indexes: [
    { name: "PRIMARY KEY", columns: ["id"], unique: true, primary: true },
    { name: "orders_customer", columns: ["customer_id", "ordered_at"], unique: false, primary: false },
    { name: "orders_email_lower", columns: ["lower(email)"], unique: false, primary: false },
  ],
  row_estimate: 2_000_000,
});

describe("queryHints: references and predicates", () => {
  it("finds tables with positions and aliases, but not INSERT targets", () => {
    const sql = "INSERT INTO t SELECT * FROM sales.events e, dim d JOIN orders o ON o.id = e.oid";
    const r = refsWithPos(sql);
    expect(r.map((x) => [x.parts.join("."), x.alias, sql.slice(x.from, x.to)])).toEqual([
      ["sales.events", "e", "sales.events"],
      ["dim", "d", "dim"],
      ["orders", "o", "orders"],
    ]);
  });

  it("collects filtered columns, wrapping functions and leading wildcards", () => {
    const u = columnUses("select * from t where date(t.ts) = '2024-01-01' and name like '%x' and id::text = '1' and case when a = 1 then b end = 2");
    expect(u.map((x) => [x.qual, x.name, x.wrappedBy, !!x.leadingWildcard])).toEqual([
      ["t", "ts", "date", false],
      [undefined, "name", undefined, true],
      [undefined, "id", "::", false],
      [undefined, "a", undefined, false],
      [undefined, "b", undefined, false],
    ]);
  });
});

describe("queryHints: rules", () => {
  it("warns when a partitioned table has no partition filter", () => {
    const h = hints("SELECT * FROM events WHERE user_id = 1", { events });
    expect(h).toHaveLength(1);
    expect(h[0]).toMatchObject({ level: "warn", text: "events" });
    expect(h[0].message).toContain("partitioned by event_date");
  });

  it("says the query is rejected when a partition filter is required", () => {
    const req = layout({ partition_by: ["day"], requires_partition_filter: true });
    expect(hints("select count(*) from logs", { logs: req })[0].message).toContain("rejected");
  });

  it("is quiet when the partition and cluster keys are filtered", () => {
    expect(hints("select * from events e where e.event_date >= '2024-01-01' and e.user_id = 7", { events })).toEqual([]);
  });

  it("flags functions on the partition column", () => {
    const h = hints("select * from events where date_trunc('month', event_date) = '2024-01-01' and user_id = 1", { events });
    expect(h).toHaveLength(1);
    expect(h[0]).toMatchObject({ level: "warn", text: "event_date" });
    expect(h[0].message).toContain("date_trunc('month', event_date)");
  });

  it("suggests the cluster key when only the partition is filtered", () => {
    const h = hints("select * from events where event_date = '2024-01-02'", { events });
    expect(h).toEqual([expect.objectContaining({ level: "info", text: "events" })]);
    expect(h[0].message).toContain("clustered by user_id");
  });

  it("flags wrapped and wildcard filters on indexed columns, except matching expression indexes", () => {
    const h = hints("select * from orders o where cast(o.id as text) = '5' or o.customer_id::text like '%9' or lower(email) = 'a@b.c'", { orders });
    expect(h.map((x) => [x.level, x.text])).toEqual([
      ["warn", "id"],
      ["warn", "customer_id"],
    ]);
    expect(h[0].message).toContain("primary key");
  });

  it("mentions a scan when no index starts with the filtered columns of a big table", () => {
    const h = hints("select * from orders where status = 'open'", { orders });
    expect(h).toHaveLength(1);
    expect(h[0].message).toContain("No index on orders (~2.0M rows) starts with status");
    expect(hints("select * from orders where customer_id = 3 and status = 'open'", { orders })).toEqual([]);
    expect(hints("select * from orders where status like '%x'", { orders: { ...orders, row_estimate: 10 } })).toEqual([]);
    expect(hints("select * from orders where lower(email) = 'a@b.c'", { orders }), "expression index").toEqual([]);
  });

  it("attributes qualified columns to the right table in joins", () => {
    const h = hints("select * from events e join orders o on o.id = e.order_id where e.event_date > '2024-01-01' and e.user_id = 1", { events, orders });
    expect(h).toEqual([]);
    const h2 = hints("select * from events e join orders o on o.id = e.order_id where o.event_date > '2024-01-01'", { events, orders });
    expect(h2.some((x) => x.text === "events" && x.level === "warn")).toBe(true);
  });

  it("mentions LIMIT on BigQuery", () => {
    expect(hints("select * from events limit 10", { events }, "bigquery")[0].message).toContain("LIMIT does not reduce bytes scanned");
  });
});

describe("queryHints: summaries", () => {
  it("describes layout and column roles", () => {
    expect(layoutSummary(events)).toEqual(["Partitioned by event_date (day)", "Clustered by user_id"]);
    expect(layoutSummary(orders)[0]).toBe("Primary key (id)");
    expect(layoutSummary(orders).at(-1)).toBe("~2.0M rows");
    expect(columnRoles(orders)).toEqual({ id: "primary key", customer_id: "indexed", "lower(email)": "indexed" });
    expect(columnRoles(events)).toEqual({ event_date: "partition key", user_id: "cluster key" });
  });
});
