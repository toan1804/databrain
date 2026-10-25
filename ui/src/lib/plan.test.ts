import { describe, expect, it } from "vitest";
import { flattenPlan, hottest, selfMs, warningsOf } from "./plan";
import type { PlanNode } from "./types";

const n = (op: string, p: Partial<PlanNode> = {}): PlanNode => ({ op, extra: [], children: [], ...p });

describe("plan", () => {
  it("splits time between a step and its children over loops", () => {
    const scan = n("Index Scan", { time_ms: 0.5, loops: 10 });
    const join = n("Nested Loop", { time_ms: 8, loops: 1, children: [n("Seq Scan", { time_ms: 2, loops: 1 }), scan] });
    expect(selfMs(scan)).toBe(5);
    expect(selfMs(join)).toBe(1);
    const rows = flattenPlan([join]);
    expect(rows.map((r) => [r.id, r.depth])).toEqual([["0", 0], ["0.0", 1], ["0.1", 1]]);
    expect(hottest(rows)?.id).toBe("0.1");
  });

  it("flags full scans of many rows and bad estimates", () => {
    expect(warningsOf(n("Seq Scan", { rows: 50_000 }))).toEqual(["Full scan of 50.0k rows"]);
    expect(warningsOf(n("Seq Scan", { rows: 10 }))).toEqual([]);
    expect(warningsOf(n("TABLE ACCESS FULL"))).toEqual(["Full scan"]);
    expect(warningsOf(n("SCAN orders"))).toEqual(["Full scan"]);
    expect(warningsOf(n("Hash Join", { rows: 10, actual_rows: 5000 }))[0]).toContain("500× more");
    expect(warningsOf(n("Hash Join", { rows: 10, actual_rows: 12 }))).toEqual([]);
    expect(warningsOf(n("Sort", { loops: 0 }))).toEqual(["Never executed"]);
  });
});
