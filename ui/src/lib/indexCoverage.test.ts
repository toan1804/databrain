import { describe, expect, it } from "vitest";
import type { DbObject, KnState } from "./types";
import { explorerTables, inScope, indexCoverage, systemSchema } from "./indexCoverage";

const state = (objects: number): KnState => ({ connection_id: "c1", indexed_at: 1, objects, schemas: ["public"], error: null });
const obj = (name: string, kind: DbObject["kind"] = "table"): DbObject => ({ schema: "public", name, kind });

describe("index coverage", () => {
  it("counts the explorer's tables and views of one connection", () => {
    const objects = {
      "c1|public": [obj("a"), obj("v", "view"), obj("f", "function"), obj("s", "sequence")],
      "c1|sales": [obj("b"), obj("m", "materialized_view")],
      "c2|public": [obj("x")],
    };
    expect(explorerTables(objects, "c1")).toBe(4);
    expect(explorerTables(objects, "c3")).toBeNull();
  });

  it("leaves out system schemas, and schemas outside a chosen index scope", () => {
    const objects = {
      "c1|public": [obj("a"), obj("b")],
      "c1|information_schema": [obj("tables", "view"), obj("columns", "view")],
      "c1|pg_catalog": [obj("pg_class")],
      "c1|sales": [obj("orders")],
      "c1|db.performance_schema": [obj("events")],
    };
    expect(explorerTables(objects, "c1")).toBe(3);
    expect(explorerTables(objects, "c1", ["public"])).toBe(2);
    expect(explorerTables(objects, "c1", ["*"])).toBe(7);
    expect(systemSchema("PG_CATALOG") && systemSchema("main.sys") && systemSchema("pg_temp_3")).toBe(true);
    expect(systemSchema("sales")).toBe(false);
    expect(inScope(["main.*"], "main.sales")).toBe(true);
    expect(inScope(["main.*"], "mainx.sales")).toBe(false);
  });

  it("says when nothing is indexed", () => {
    expect(indexCoverage(null, null)).toEqual({ kind: "none" });
    expect(indexCoverage(state(0), 300)).toEqual({ kind: "none" });
  });

  it("says when far fewer tables are indexed than the explorer lists", () => {
    expect(indexCoverage(state(12), 340)).toEqual({ kind: "few", indexed: 12, total: 340 });
    expect(indexCoverage(state(200), 340)).toBeNull();
    expect(indexCoverage(state(3), 10)).toBeNull();
    expect(indexCoverage(state(12), null)).toBeNull();
  });
});
