import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/plugin-clipboard-manager", () => ({ writeText: async () => {} }));
const { formatStat, summaryItems } = await import("./SelectionTools");

describe("selection summary", () => {
  it("lists count and distinct, and number stats when there are numbers", () => {
    const base = { cells: 6, non_null: 5, distinct: 4, distinct_capped: false, numbers: 0, sum: null, min: null, max: null, avg: null };
    expect(summaryItems(base).map((i) => i.label)).toEqual(["Count", "Distinct"]);
    const nums = summaryItems({ ...base, numbers: 5, sum: 1234.5, avg: 246.9, min: -3, max: 1000 });
    expect(nums.map((i) => [i.label, i.shown, i.copy])).toEqual([
      ["Count", "5", "5"],
      ["Distinct", "4", "4"],
      ["Sum", "1,234.5", "1234.5"],
      ["Avg", "246.9", "246.9"],
      ["Min", "-3", "-3"],
      ["Max", "1,000", "1000"],
    ]);
    expect(summaryItems({ ...base, distinct_capped: true })[1].shown).toBe("4+");
  });
  it("formats numbers like a spreadsheet status bar", () => {
    expect(formatStat(0.1 + 0.2)).toBe("0.3");
    expect(formatStat(1e6)).toBe("1,000,000");
  });
});
