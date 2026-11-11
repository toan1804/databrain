import { describe, expect, it } from "vitest";
import type { DbObject, KnState } from "./types";
import { explorerTables, indexCoverage } from "./indexCoverage";

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
