import { describe, expect, it } from "vitest";
import { buildScope, estimateQueries, initialSelection, planCatalogs, selectedObjects } from "./indexScope";
import type { IndexPlan, PlanSchema } from "./types";

const s = (catalog: string, schema: string, objects: number, extra: Partial<PlanSchema> = {}): PlanSchema => ({
  name: `${catalog}.${schema}`,
  catalog,
  is_default: false,
  system: false,
  objects,
  selected: true,
  ...extra,
});

const plan = (schemas: PlanSchema[], scope: string[] = [], large = true): IndexPlan => ({
  schemas,
  scope,
  catalogs: new Set(schemas.map((x) => x.catalog)).size,
  total_objects: schemas.reduce((n, x) => n + (x.objects ?? 0), 0),
  large,
  batch: 25,
});

const p = plan([
  s("main", "sales", 10, { is_default: true }),
  s("main", "crm", 5),
  s("main", "information_schema", 40, { system: true, selected: false }),
  s("dev", "a", 100),
  s("dev", "b", 200),
]);

describe("index scope", () => {
  it("groups catalogs and sums objects without system schemas", () => {
    const rows = planCatalogs(p);
    expect(rows.map((r) => [r.name, r.schemas.length, r.objects])).toEqual([
      ["main", 2, 15],
      ["dev", 2, 300],
    ]);
  });

  it("starts large, never-chosen plans with the default schema only", () => {
    expect([...initialSelection(p)]).toEqual(["main.sales"]);
    expect(initialSelection(plan(p.schemas, [], false)).size).toBe(4);
    const saved = plan(p.schemas.map((x) => ({ ...x, selected: x.catalog === "dev" })), ["dev.*"]);
    expect([...initialSelection(saved)]).toEqual(["dev.a", "dev.b"]);
  });

  it("builds compact scopes", () => {
    expect(buildScope(p, new Set(["main.sales", "main.crm", "dev.a", "dev.b"]))).toEqual(["*"]);
    expect(buildScope(p, new Set(["dev.a", "dev.b", "main.sales"]))).toEqual(["main.sales", "dev.*"]);
    expect(buildScope(p, new Set(["main.information_schema", "dev.a"]))).toEqual(["dev.a", "main.information_schema"]);
    expect(buildScope(p, new Set())).toEqual([]);
  });

  it("estimates queries and tables", () => {
    const all = new Set(["main.sales", "main.crm", "dev.a", "dev.b"]);
    expect(estimateQueries("databricks", p, all)).toBe(4);
    expect(estimateQueries("snowflake", p, all)).toBe(8);
    // 1,000 schemas in one catalog: 25 per batch → 80 queries; 200 → 10;
    // 500 → 12 (each IN list holds at most 200 schemas).
    const big = plan(Array.from({ length: 1000 }, (_, i) => s("lake", `s${i}`, 1)));
    const every = new Set(big.schemas.map((x) => x.name));
    expect(estimateQueries("databricks", big, every)).toBe(80);
    expect(estimateQueries("databricks", big, every, 200)).toBe(10);
    expect(estimateQueries("databricks", big, every, 500)).toBe(12);
    expect(estimateQueries("databricks", big, every, 0)).toBe(80);
    expect(selectedObjects(p, new Set(["dev.a", "main.crm"]))).toBe(105);
    expect(selectedObjects(plan([{ ...s("x", "y", 0), objects: null }]), new Set(["x.y"]))).toBeNull();
  });
});
