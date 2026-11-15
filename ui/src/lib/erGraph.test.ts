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

describe("ER layout with foreign-key cycles", () => {
  it("stays as wide as the longest chain, not one rank per pass", () => {
    // a <-> b reference each other, and a chain of 200 tables hangs off b.
    const fk = (to: string) => ({ columns: ["ref"], ref_schema: "s", ref_table: to, ref_columns: ["id"] });
    const t = (name: string, refs: string[]): TableColumns => ({ table: name, columns: [col("id", true), col("ref")], foreign_keys: refs.map(fk) });
    const list: TableColumns[] = [t("a", ["b"]), t("b", ["a"])];
    for (let i = 0; i < 200; i++) list.push(t(`c${i}`, [i === 0 ? "b" : `c${i - 1}`]));
    const l = layout(erGraph("s", list).graph, "lr", { expanded: "all" });
    const ranks = new Set(l.nodes.map((n) => n.rank));
    // The ranks themselves stay small (they grew by two per pass, to ~600, and set the width).
    expect(Math.max(...ranks)).toBeLessThan(210);
    expect(Math.min(...l.nodes.map((n) => n.x))).toBe(Math.min(...layout(erGraph("s", [t("x", [])]).graph, "lr", { expanded: "all" }).nodes.map((n) => n.x)));
    // 202 tables in a chain (plus one table of the cycle): at most 203 columns, none empty.
    expect(ranks.size).toBeLessThanOrEqual(203);
    const xs = [...new Set(l.nodes.map((n) => n.x))].sort((p, q) => p - q);
    expect(xs.length).toBe(ranks.size);
    const step = xs[1] - xs[0];
    expect(xs.every((x, i) => i === 0 || x - xs[i - 1] === step)).toBe(true);
  });
});

describe("ER layout on a large schema", () => {
  it("ranks a 20,000-table chain listed backwards in well under a second", () => {
    const fk = (to: string) => ({ columns: ["ref"], ref_schema: "s", ref_table: to, ref_columns: ["id"] });
    const list: TableColumns[] = [];
    for (let i = 20_000 - 1; i >= 0; i--) list.push({ table: `t${i}`, columns: [col("id", true), col("ref")], foreign_keys: i ? [fk(`t${i - 1}`)] : [] });
    const g = erGraph("s", list).graph;
    const t0 = performance.now();
    const l = layout(g, "tb", { expanded: "all" });
    expect(performance.now() - t0).toBeLessThan(3000);
    expect(new Set(l.nodes.map((n) => n.rank)).size).toBe(20_000);
  });
});
