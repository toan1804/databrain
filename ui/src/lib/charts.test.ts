import { describe, expect, it } from "vitest";
import { pieSlices, pinOf, scatterPoints, specFor, specForKind } from "./charts";
import type { ChartData, ColumnMeta } from "./types";

const cols = (names: string[]) => names.map((name) => ({ name, family: "text" }) as unknown as ColumnMeta);

describe("charts", () => {
  it("pins by column name and maps back after a re-run", () => {
    const pin = pinOf({ x: 1, y: [2], agg: "sum", series: null, limit: 25 }, "pie", cols(["id", "country", "amount"]), { id: "p", title: "t", connection_id: "c", sql: "q" });
    expect([pin.x, pin.y, pin.kind]).toEqual(["country", ["amount"], "pie"]);
    expect(specFor(pin, cols(["amount", "country"]))).toEqual({ x: 1, y: [0], agg: "sum", series: null, limit: 25 });
    expect(specFor(pin, cols(["country"]))).toBeNull();
  });

  it("adapts the spec to the kind", () => {
    const s = { x: 0, y: [1, 2], agg: "sum" as const, series: 3, limit: 50 };
    expect(specForKind(s, "scatter")).toMatchObject({ agg: "none", series: null });
    expect(specForKind(s, "pie")).toMatchObject({ y: [1], series: null });
  });

  it("makes pie slices with an Other slice", () => {
    const data: ChartData = { x: ["a", "b", "c", "d"], x_family: "text", series: [{ name: "v", values: [5, 3, 1, 1] }], truncated: false, rows: 4 };
    const s = pieSlices(data, 3);
    expect(s.map((x) => [x.label, x.value])).toEqual([["a", 5], ["b", 3], ["Other", 2]]);
    expect(s[0].share).toBeCloseTo(0.5);
  });

  it("reads scatter points from numbers and dates", () => {
    const n: ChartData = { x: ["1", "2", "x"], x_family: "number", series: [{ name: "y", values: [10, null, 3] }], truncated: false, rows: 3 };
    expect(scatterPoints(n).map((p) => [p.x, p.y])).toEqual([[1, 10]]);
    const d: ChartData = { x: ["2026-01-02 03:04:05"], x_family: "date", series: [{ name: "y", values: [1] }], truncated: false, rows: 1 };
    expect(scatterPoints(d)[0].x).toBe(Date.parse("2026-01-02T03:04:05"));
  });
});
