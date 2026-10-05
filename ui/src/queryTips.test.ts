import { describe, expect, it, vi } from "vitest";
import type { StatementKind } from "./lib/types";

vi.mock("./lib/api", () => ({ api: {} }));

const plan = (index: number, sql: string, start: number, kind: StatementKind = "read") => ({
  plan: { index, sql, start, end: start + sql.length, classification: { kind, missing_where: false, keyword: "SELECT" } },
  status: "done",
});

describe("queryTips", () => {
  it("analyses finished statements with layouts resolved by the backend", async () => {
    const { computeRunTips, clearTipCache } = await import("./queryTips");
    clearTipCache();
    const calls: string[][] = [];
    const fetch = async (_c: string, tables: string[]) => {
      calls.push(tables);
      return tables.map((w) =>
        w === "sales.events"
          ? { written: w, layout: { indexes: [], partition_by: ["day"], cluster_by: [], requires_partition_filter: false, notes: [] } }
          : { written: w, error: "table not found" },
      );
    };
    const doc = "select 1;\nselect * from sales.events where user_id = 1;";
    const s2 = "select * from sales.events where user_id = 1";
    const r = await computeRunTips("c", "postgres", [plan(0, "select 1", 0), plan(1, s2, 10), { ...plan(2, "select * from ghost", 60), status: "error" }], fetch);
    expect(r.tips).toHaveLength(1);
    expect(doc.slice(r.tips[0].from, r.tips[0].to)).toBe("sales.events");
    expect(r.tips[0].message).toContain("partitioned by day");
    expect(r.tables[0].title).toBe("sales.events");
    // Failed statements are not analysed.
    expect(calls).toEqual([["sales.events"]]);
    // Cached: a second run does not ask again.
    await computeRunTips("c", "postgres", [plan(0, s2, 0)], fetch);
    expect(calls).toHaveLength(1);
    const miss = await computeRunTips("c", "postgres", [plan(0, "select * from ghost g where g.a = 1", 0)], fetch);
    expect(miss.unavailable).toEqual([{ table: "ghost", error: "table not found" }]);
    const ddl = await computeRunTips("c", "postgres", [plan(0, "create table x as select * from sales.events", 0, "ddl")], fetch);
    expect(ddl.tips).toEqual([]);
  });
});
