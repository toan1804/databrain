import { describe, expect, it } from "vitest";
import {
  comboMatches,
  errorRange,
  formatDuration,
  fuzzyMatch,
  placeComboList,
  quoteIdent,
  selectTopSql,
  toggleSort,
  upsertFilter,
} from "./util";

describe("comboMatches", () => {
  const models = ["gpt-4o", "gpt-4o-mini", "claude-sonnet-4"];
  it("lists every option when empty or when one is picked, else the matching ones", () => {
    expect(comboMatches(models, "")).toEqual(models);
    expect(comboMatches(models, "  ")).toEqual(models);
    expect(comboMatches(models, "GPT-4o")).toEqual(models);
    expect(comboMatches(models, "mini")).toEqual(["gpt-4o-mini"]);
    expect(comboMatches(models, "o1")).toEqual([]);
  });
});

describe("placeComboList", () => {
  const win = { width: 1200, height: 800 };
  const overlaps = (f: { top: number; bottom: number }, p: ReturnType<typeof placeComboList>) => {
    const top = p.top ?? win.height - p.bottom! - p.maxHeight;
    const bottom = p.top !== undefined ? p.top + p.maxHeight : win.height - p.bottom!;
    return top < f.bottom && bottom > f.top;
  };
  it("goes under a field with room below", () => {
    const f = { left: 100, top: 100, bottom: 124, width: 300 };
    const p = placeComboList(f, win);
    expect(p).toMatchObject({ top: 126, left: 100, width: 300, maxHeight: 260 });
    expect(overlaps(f, p)).toBe(false);
  });
  it("goes above a field at the bottom of the window (AI panel footer), inside the window", () => {
    const f = { left: 1100, top: 770, bottom: 794, width: 120 };
    const p = placeComboList(f, win);
    expect(p.top).toBeUndefined();
    expect(p.bottom).toBe(win.height - 770 + 2);
    expect(overlaps(f, p)).toBe(false);
    expect(p.left + p.width).toBeLessThanOrEqual(win.width - 4);
    expect(p.width).toBe(220);
  });
  it("is never wider than the window", () => {
    const p = placeComboList({ left: 0, top: 10, bottom: 30, width: 50 }, { width: 150, height: 400 });
    expect(p.left).toBe(4);
    expect(p.width).toBe(142);
  });
});

describe("toggleSort", () => {
  it("cycles asc -> desc -> off", () => {
    let s = toggleSort([], 2, false);
    expect(s).toEqual([{ column: 2, descending: false }]);
    s = toggleSort(s, 2, false);
    expect(s).toEqual([{ column: 2, descending: true }]);
    expect(toggleSort(s, 2, false)).toEqual([]);
  });

  it("replaces on plain click and appends on shift-click", () => {
    const s = [{ column: 1, descending: false }];
    expect(toggleSort(s, 3, false)).toEqual([{ column: 3, descending: false }]);
    expect(toggleSort(s, 3, true)).toEqual([
      { column: 1, descending: false },
      { column: 3, descending: false },
    ]);
    expect(toggleSort([...s, { column: 3, descending: false }], 1, true)).toEqual([
      { column: 1, descending: true },
      { column: 3, descending: false },
    ]);
  });
});

describe("errorRange", () => {
  const stmt = { start: 10, end: 30, sql: "select * frm users" };
  it("underlines the token at the position", () => {
    expect(errorRange(stmt, 10)).toEqual({ from: 19, to: 22 });
  });
  it("falls back to the whole statement", () => {
    expect(errorRange(stmt, undefined)).toEqual({ from: 10, to: 30 });
  });
  it("handles astral characters", () => {
    const s = { start: 0, end: 20, sql: "select '😀', bad" };
    // position 13 (1-based, code points) is "b"; the emoji is 2 UTF-16 units
    expect(errorRange(s, 13)).toEqual({ from: 13, to: 16 });
  });
});

describe("misc", () => {
  it("upserts filters by column+op", () => {
    const f = upsertFilter([{ column: 0, op: "equals", value: "a" }], {
      column: 0,
      op: "equals",
      value: "b",
    });
    expect(f).toEqual([{ column: 0, op: "equals", value: "b" }]);
  });
  it("quotes identifiers only when needed", () => {
    expect(quoteIdent("postgres", "users")).toBe("users");
    expect(quoteIdent("postgres", "Order Items")).toBe('"Order Items"');
    expect(quoteIdent("mysql", "a`b")).toBe("`a``b`");
    expect(selectTopSql("sqlite", "main", "t")).toBe("SELECT *\nFROM main.t\nLIMIT 100;");
  });
  it("formats durations", () => {
    expect(formatDuration(12)).toBe("12 ms");
    expect(formatDuration(1234)).toBe("1.23 s");
    expect(formatDuration(125_000)).toBe("2m 5s");
  });
  it("fuzzy matches", () => {
    expect(fuzzyMatch("nqt", "New query tab")).toBe(true);
    expect(fuzzyMatch("xyz", "New query tab")).toBe(false);
  });
});

describe("dialect quoting", () => {
  it("quotes per dialect", async () => {
    const { quoteIdent, selectTopSql, qualifiedName } = await import("./util");
    expect(quoteIdent("mssql", "Order Details")).toBe("[Order Details]");
    expect(quoteIdent("oracle", "EMP")).toBe("EMP");
    expect(quoteIdent("oracle", "emp")).toBe('"emp"');
    expect(quoteIdent("databricks", "my-col")).toBe("`my-col`");
    expect(qualifiedName("databricks", "main.sales", "orders")).toBe("main.sales.orders");
    expect(qualifiedName("bigquery", "proj-1.ds", "t")).toBe("`proj-1.ds.t`");
    expect(qualifiedName("duckdb", "memory.files", "sales")).toBe("memory.files.sales");
    expect(selectTopSql("mssql", "dbo", "t", 5)).toBe("SELECT TOP 5 *\nFROM dbo.t;");
    expect(selectTopSql("oracle", "HR", "EMP", 5)).toBe("SELECT *\nFROM HR.EMP\nFETCH FIRST 5 ROWS ONLY");
  });
});

describe("lineDiff", () => {
  it("marks added and removed lines", async () => {
    const { lineDiff } = await import("./util");
    const d = lineDiff("select a\nfrom t\nwhere x", "select a, b\nfrom t\nwhere x");
    expect(d).toEqual([
      { op: "del", text: "select a" },
      { op: "add", text: "select a, b" },
      { op: "same", text: "from t" },
      { op: "same", text: "where x" },
    ]);
    expect(lineDiff("", "x").filter((l) => l.op === "add").length).toBe(1);
  });
});

describe("pageRange", () => {
  it("never returns negative or out-of-range pages", async () => {
    const { pageRange } = await import("./util");
    expect(pageRange(-40, 30, 200, 1000)).toEqual([0]);
    expect(pageRange(-500, 900, 200, 1000)).toEqual([0, 1, 2]);
    expect(pageRange(390, 30, 200, 1000)).toEqual([1, 2]);
    expect(pageRange(950, 400, 200, 1000)).toEqual([4]);
    expect(pageRange(0, 40, 200, 0)).toEqual([0]);
    expect(pageRange(NaN, NaN, 200, 1000)).toEqual([0]);
  });
});
