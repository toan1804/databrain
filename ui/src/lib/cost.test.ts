import { describe, expect, it } from "vitest";
import { costLevel, costSummary, fmtBytes, LARGE_SCAN_BYTES } from "./cost";
import type { CostEstimate } from "./types";

const est = (p: Partial<CostEstimate>): CostEstimate => ({ method: "dry run", cache_hit: false, notes: [], ...p });

describe("cost preview", () => {
  it("formats bytes in binary units", () => {
    expect(fmtBytes(0)).toBe("0 B");
    expect(fmtBytes(1536)).toBe("1.5 KiB");
    expect(fmtBytes(5 * 1024 ** 3)).toBe("5.0 GiB");
    expect(fmtBytes(300 * 1024 ** 4)).toBe("300 TiB");
  });

  it("summarizes each engine's estimate", () => {
    expect(costSummary(est({ bytes: 1024 ** 3 }))).toBe("scans 1.0 GiB");
    expect(costSummary(est({ bytes: 9, cache_hit: true }))).toBe("from cache, 0 B billed");
    expect(costSummary(est({ method: "EXPLAIN USING JSON", bytes: 2048, partitions_assigned: 3, partitions_total: 1200 }))).toBe("scans 2.0 KiB · 3 of 1,200 partitions");
    expect(costSummary(est({ method: "EXPLAIN COST", bytes: 1024 ** 2, rows: 50 }))).toBe("reads about 1.0 MiB · ~50 rows");
    expect(costSummary(est({ method: "EXPLAIN COST" }))).toBe("no size estimate");
  });

  it("warns on the byte cap, large scans and no pruning", () => {
    expect(costLevel(est({ bytes: 10 }))).toBe("info");
    expect(costLevel(est({ bytes: 10, over_limit: 5 }))).toBe("warn");
    expect(costLevel(est({ bytes: LARGE_SCAN_BYTES }))).toBe("warn");
    expect(costLevel(est({ bytes: LARGE_SCAN_BYTES, cache_hit: true }))).toBe("info");
    expect(costLevel(est({ partitions_assigned: 8, partitions_total: 8 }))).toBe("warn");
    expect(costLevel(est({ partitions_assigned: 1, partitions_total: 1 }))).toBe("info");
  });
});
