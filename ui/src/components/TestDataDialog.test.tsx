import { describe, expect, it } from "vitest";
import type { TestDataColumn } from "../lib/types";
import { suggestTable, testDataTable } from "./LoadIntoDialog";
import { defaultColumns, MAX_SEED, seedProblem, specFor } from "./TestDataDialog";

const col = (name: string, p: Partial<TestDataColumn> = {}): TestDataColumn => ({
  name,
  data_type: "int",
  nullable: true,
  primary_key: false,
  supported: true,
  database_fills: false,
  generator: "",
  ai_text: false,
  ai_ok: false,
  ...p,
});

describe("test data dialog", () => {
  const cols = [col("id", { database_fills: true, primary_key: true }), col("name"), col("note"), col("shape", { supported: false })];

  it("ticks what DataBrain fills and the database doesn't", () => {
    expect([...defaultColumns(cols)]).toEqual(["name", "note"]);
  });

  it("sends unticked columns as skip and ticked database-filled ones as fill", () => {
    const s = specFor(cols, new Set(["id", "name"]), 250_000, 140, " 42 ", false);
    expect(s).toEqual({ rows: 100_000, null_percent: 100, seed: 42, skip: ["note"], fill: ["id"], parent_keys: false, patterns: {} });
    expect(specFor(cols, new Set(), 0, -5, "abc", true)).toMatchObject({ rows: 1, null_percent: 0, seed: null });
  });

  it("sends every column's pattern: the user's, else its default, empty for none", () => {
    const pcols = [
      col("id", { primary_key: true }),
      col("age", { pattern: "18..90", pattern_hint: "18..90" }),
      col("email", { pattern_hint: "%@gmail.com" }),
      col("born", { pattern: "1950..2005", pattern_hint: "1950-01-01..2005-12-31" }),
    ];
    const s = specFor(pcols, new Set(["age", "email", "born"]), 10, 0, "", true, { email: " %@corp.vn ", born: "" });
    expect(s.patterns).toEqual({ age: "18..90", email: "%@corp.vn", born: "" });
  });

  it("sends only seeds that a JavaScript number keeps exactly", () => {
    expect(specFor(cols, new Set(), 1, 0, String(MAX_SEED), true).seed).toBe(9007199254740991);
    expect(seedProblem("9007199254740991")).toBeNull();
    expect(seedProblem("9007199254740993")).toMatch(/up to 9007199254740991/);
    expect(specFor(cols, new Set(), 1, 0, "9007199254740993", true).seed).toBeNull();
    expect(seedProblem("")).toBeNull();
  });

  it("loads generated rows back into their table by default", () => {
    const sql = '-- Test data for "public"."orders" (50 rows, seed 3)';
    expect(testDataTable(sql)).toBe('"public"."orders"');
    expect(suggestTable(sql, null)).toBe('"public"."orders"');
    expect(testDataTable("select * from orders")).toBeNull();
    expect(suggestTable("select * from sales.orders")).toBe("orders");
  });
});
