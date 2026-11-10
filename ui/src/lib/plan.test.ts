import { describe, expect, it } from "vitest";
import { comparePlans, delta, flattenPlan, fmtDelta, hottest, selfMs, warningsOf } from "./plan";
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

describe("plan comparison", () => {
  it("compares cost, steps and full scans of a rewrite", () => {
    const before = [n("Hash Join", { cost: 1000, rows: 10, children: [n("Seq Scan", { object: "on orders", rows: 90_000, cost: 800 }), n("Seq Scan", { object: "on customers", rows: 5000, cost: 150 })] })];
    const after = [n("Nested Loop", { cost: 120, rows: 10, children: [n("Index Scan", { object: "using orders_cust_idx on orders", rows: 40, cost: 60 }), n("Seq Scan", { object: "on customers", rows: 5000, cost: 50 })] })];
    const c = comparePlans({ roots: before }, { roots: after });
    expect(c.cost).toMatchObject({ before: 1000, after: 120, better: true });
    expect(fmtDelta(c.cost)).toBe("−88%");
    expect(c.scansRemoved).toEqual(["Seq Scan on orders"]);
    expect(c.scansAdded).toEqual([]);
    expect(c.scans).toMatchObject({ before: 2, after: 1, better: true });
    // Same step on both sides (customers scan) is not marked; the others are.
    expect([...c.gone].sort()).toEqual(["0", "0.0"]);
    expect([...c.added].sort()).toEqual(["0", "0.0"]);
    expect(c.time.better).toBeNull();
  });

  it("measures time when analyzed and says when nothing changed", () => {
    const c = comparePlans({ roots: [n("Seq Scan", { time_ms: 50 })], execution_ms: 80 }, { roots: [n("Seq Scan", { time_ms: 20 })], execution_ms: 200 });
    expect(c.time).toMatchObject({ before: 80, after: 200, better: false });
    expect(fmtDelta(c.time)).toBe("2.5×");
    expect(fmtDelta(delta(100, 100.5))).toBe("same");
    expect(fmtDelta(delta(undefined, 3))).toBe("");
    expect(fmtDelta(delta(10, 13))).toBe("+30%");
    expect(delta(5, 9, false).better).toBe(true);
  });
});
