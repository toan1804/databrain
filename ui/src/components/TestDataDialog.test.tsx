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
  ...p,
});

describe("test data dialog", () => {
  const cols = [col("id", { database_fills: true, primary_key: true }), col("name"), col("note"), col("shape", { supported: false })];

  it("ticks what DataBrain fills and the database doesn't", () => {
    expect([...defaultColumns(cols)]).toEqual(["name", "note"]);
  });

  it("sends unticked columns as skip and ticked database-filled ones as fill", () => {
    const s = specFor(cols, new Set(["id", "name"]), 250_000, 140, " 42 ", false);
    expect(s).toEqual({ rows: 100_000, null_percent: 100, seed: 42, skip: ["note"], fill: ["id"], parent_keys: false });
    expect(specFor(cols, new Set(), 0, -5, "abc", true)).toMatchObject({ rows: 1, null_percent: 0, seed: null });
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
