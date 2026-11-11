import { describe, expect, it } from "vitest";
import { erGraph } from "./erGraph";
import { layout } from "./lineageLayout";
import { exportText, moreLabel, toSvg } from "./lineageExport";
import type { TableColumns } from "./types";

const col = (name: string, pk = false) => ({ name, data_type: "int", nullable: !pk, is_primary_key: pk });
const tables: TableColumns[] = [
  { table: "customers", columns: [col("id", true), col("name"), col("email")], foreign_keys: [] },
  { table: "orders", columns: [col("id", true), col("customer_id"), col("amount")], foreign_keys: [{ columns: ["customer_id"], ref_schema: "sales", ref_table: "customers", ref_columns: ["id"] }] },
];

describe("ER diagram layout", () => {
  // The ER tab's display: no `usedOnly`, so tables are not trimmed to "used" columns.
  const er = erGraph("sales", tables).graph;

  it("lists every column of every table, with no unused-columns row", () => {
    const l = layout(er, "lr", { expanded: "all" });
    expect(l.byId.get("sales.customers")!.columns.map((c) => c.name)).toEqual(["id", "name", "email"]);
    expect(l.byId.get("sales.orders")!.columns.map((c) => c.name)).toEqual(["id", "customer_id", "amount"]);
    for (const n of l.nodes) {
      expect(n.hidden).toBe(0);
      expect(n.full).toBe(false);
      expect(moreLabel(n, true)).toBe("");
    }
    const svg = toSvg(er, l, { inner: true });
    expect(svg).not.toContain("unused");
    expect(svg).not.toContain("data-more");
  });

  it("exports include the columns", () => {
    const l = layout(er, "lr", { expanded: "all" });
    for (const format of ["svg", "drawio", "mermaid", "dot"] as const) {
      const text = exportText(format, er, l);
      for (const c of ["name", "email", "customer_id", "amount"]) expect(text, `${format}: ${c}`).toContain(c);
      expect(text).not.toContain("unused");
    }
  });

  it("query lineage still trims tables when it opts in", () => {
    const l = layout(er, "lr", { expanded: "all", usedOnly: true });
    // erGraph never marks columns used, so a lineage-style display hides them all.
    expect(l.byId.get("sales.customers")!.columns).toEqual([]);
    expect(moreLabel(l.byId.get("sales.customers")!)).toBe("+3 unused columns");
  });
});
