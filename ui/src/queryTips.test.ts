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

describe("slow statements", () => {
  it("picks statements running or done after the threshold", async () => {
    const { slowStatements } = await import("./queryTips");
    const now = 1_000_000;
    const st = [
      { id: "fast", status: "done", durationMs: 2_000 },
      { id: "slow", status: "done", durationMs: 61_000 },
      { id: "long-running", status: "running", startedAt: now - 60_000 },
      { id: "just-started", status: "running", startedAt: now - 5_000 },
      { id: "failed", status: "error", durationMs: 90_000 },
    ];
    expect(slowStatements(st, 60_000, now).map((s) => s.id)).toEqual(["slow", "long-running"]);
    expect(slowStatements(st, 1_000, now).map((s) => s.id)).toEqual(["fast", "slow", "long-running", "just-started"]);
    expect(slowStatements(st, 0, now)).toEqual([]);
  });

  it("computes tips for a statement that is still running", async () => {
    const { computeRunTips, clearTipCache } = await import("./queryTips");
    clearTipCache();
    const sql = "select * from public.events where date(created_at) = current_date";
    const plan = { index: 0, sql, start: 0, end: sql.length, classification: { kind: "read" as const, missing_where: false, keyword: "SELECT" } };
    const layout = { indexes: [], partition_by: ["created_at"], cluster_by: [], requires_partition_filter: false, notes: [] };
    const r = await computeRunTips("c", "postgres", [{ plan, status: "running" }], async () => [{ written: "public.events", layout }]);
    expect(r.tips.length).toBeGreaterThan(0);
  });
});
