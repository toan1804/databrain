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

  it("keeps a full scan that a selective WHERE only made look small", () => {
    // PostgreSQL: Plan Rows of a Seq Scan are the rows after its filter, not the rows read.
    const before = [n("Seq Scan", { object: "on public.orders", rows: 50_000, cost: 900 })];
    const after = [n("Seq Scan", { object: "on public.orders o", rows: 200, cost: 1025, extra: [["Filter", "(status = 'open'::text)"]] })];
    const c = comparePlans({ roots: before }, { roots: after });
    expect(c.scansRemoved).toEqual([]);
    expect(c.scansAdded).toEqual([]);
    expect(c.scans).toMatchObject({ before: 1, after: 1, better: null });
    // And the other way round: a filter dropped is not a "new" scan either.
    const back = comparePlans({ roots: after }, { roots: before });
    expect(back.scansRemoved).toEqual([]);
    expect(back.scansAdded).toEqual([]);
    expect(back.scans.better).toBeNull();
  });

  it("counts a full scan the same whatever the engine calls it", () => {
    // Parallel Seq Scan → Seq Scan of the same table: still there.
    const par = comparePlans({ roots: [n("Gather", { children: [n("Parallel Seq Scan", { object: "on orders", rows: 40_000 })] })] }, { roots: [n("Seq Scan", { object: "on orders", rows: 300 })] });
    expect(par.scansRemoved).toEqual([]);
    expect(par.scans.better).toBeNull();
    // Oracle and SQL Server: filtered estimates on full scans.
    const ora = comparePlans({ roots: [n("TABLE ACCESS FULL", { object: "on APP.ORDERS", rows: 50_000 })] }, { roots: [n("TABLE ACCESS FULL", { object: "on APP.ORDERS", rows: 200 })] });
    expect(ora.scansRemoved).toEqual([]);
    expect(ora.scans).toMatchObject({ before: 1, after: 1 });
    // A full scan that became an index lookup is gone.
    const idx = comparePlans({ roots: [n("TABLE ACCESS FULL", { object: "on APP.ORDERS", rows: 50_000 })] }, { roots: [n("INDEX RANGE SCAN", { object: "on APP.ORDERS_STATUS", rows: 200 })] });
    expect(idx.scansRemoved).toEqual(["TABLE ACCESS FULL on APP.ORDERS"]);
    expect(idx.scans).toMatchObject({ before: 1, after: 0, better: true });
    // Small scans on both sides stay out of it; a big new one is new.
    const small = comparePlans({ roots: [n("Seq Scan", { object: "on flags", rows: 5 })] }, { roots: [n("Seq Scan", { object: "on flags", rows: 5 }), n("Seq Scan", { object: "on events", rows: 80_000 })] });
    expect(small.scans).toMatchObject({ before: 0, after: 1, better: false });
    expect(small.scansAdded).toEqual(["Seq Scan on events"]);
  });

  it("counts the rows a filter dropped as read when analyzed", () => {
    const scan = n("Seq Scan", { object: "on orders", rows: 200, actual_rows: 180, loops: 1, extra: [["Rows Removed by Filter", "49820"]] });
    expect(warningsOf(scan)[0]).toBe("Full scan of 50.0k rows");
  });
});
