import { describe, expect, it } from "vitest";
import type { HistoryEntry } from "./bindings/HistoryEntry";
import type { LearnedJoinView } from "./bindings/LearnedJoinView";
import { inboundKeys, joinsOf, mentions, queryUses, sameTable } from "./tableRelations";

const join = (id: number, left: string, lc: string[], right: string, rc: string[]): LearnedJoinView => ({
  shown: true,
  hidden_reason: null,
  id,
  left_table: left,
  left_columns: lc,
  right_table: right,
  right_columns: rc,
  status: "learned",
  declared: false,
  queries: 2,
  days: 2,
  ai_queries: 0,
  kinds: {},
  first_seen: 0,
  last_seen: 0,
  documented: null,
});

const entry = (id: number, sql: string, at: number, extra: Partial<HistoryEntry> = {}): HistoryEntry => ({
  id,
  origin: "user",
  connection_id: "c",
  connection_name: "C",
  sql,
  started_at: at,
  duration_ms: 1,
  rows: 1,
  status: "success",
  error: null,
  output_handle: null,
  result_id: null,
  ...extra,
});

describe("table explorer: relations and usage", () => {
  it("knows a table under the knowledge base's names (any case, with a catalog)", () => {
    expect(sameTable("sales.orders", "sales", "orders")).toBe(true);
    expect(sameTable("SALES.Orders", "sales", "ORDERS")).toBe(true);
    expect(sameTable("shop.sales.orders", "shop.sales", "orders")).toBe(true);
    expect(sameTable("shop.sales.orders", "sales", "orders")).toBe(true);
    expect(sameTable("sales.orders_old", "sales", "orders")).toBe(false);
    expect(sameTable("hr.orders", "sales", "orders")).toBe(false);
  });

  it("finds the keys of the schema's tables that point at it", () => {
    const tables = [
      { table: "items", columns: [], foreign_keys: [{ columns: ["order_id"], ref_schema: "sales", ref_table: "orders", ref_columns: ["id"] }] },
      { table: "notes", columns: [], foreign_keys: [{ columns: ["order_id"], ref_schema: "", ref_table: "Orders", ref_columns: ["id"] }] },
      { table: "other", columns: [], foreign_keys: [{ columns: ["x"], ref_schema: "hr", ref_table: "orders", ref_columns: ["id"] }] },
    ];
    expect(inboundKeys(tables, "sales", "orders").map((k) => k.table)).toEqual(["items", "notes"]);
    expect(inboundKeys(tables, "db.sales", "orders").map((k) => k.table)).toEqual(["items", "notes"]);
  });

  it("gives each join from the table's side", () => {
    const joins = [join(1, "sales.customers", ["id"], "sales.orders", ["customer_id"]), join(2, "sales.orders", ["id"], "sales.items", ["order_id"]), join(3, "sales.a", ["x"], "sales.b", ["y"])];
    expect(joinsOf(joins, "sales", "orders").map((j) => [j.columns, j.other, j.otherColumns])).toEqual([
      [["customer_id"], "sales.customers", ["id"]],
      [["id"], "sales.items", ["order_id"]],
    ]);
  });

  it("finds queries that name the table as a word, grouped by query, most runs first", () => {
    expect(mentions("select * from sales.orders o", "orders")).toBe(true);
    expect(mentions('select * from "Orders"', "orders")).toBe(true);
    expect(mentions("select * from orders_old", "orders")).toBe(false);
    expect(mentions("select * from preorders", "orders")).toBe(false);
    expect(mentions("select * from t where x = '$orders'", "orders")).toBe(false);
    const uses = queryUses(
      [
        entry(1, "select * from orders", 10),
        entry(2, "SELECT *\n  FROM orders;", 30, { origin: "job" }),
        entry(3, "select count(*) from orders", 20, { status: "error" }),
        entry(4, "select * from orders_old", 40),
      ],
      "orders",
    );
    expect(uses.map((u) => [u.runs, u.last, u.origins, u.failed])).toEqual([
      [2, 30, ["user", "job"], 0],
      [1, 20, ["user"], 1],
    ]);
    expect(uses[0].sql).toBe("SELECT *\n  FROM orders;");
  });
});
