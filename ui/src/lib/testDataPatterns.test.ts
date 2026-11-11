import { describe, expect, it } from "vitest";
import { parseStoredPatterns, patternOf, patternsKey } from "./testDataPatterns";
import type { TestDataColumn } from "./types";

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

describe("test data patterns", () => {
  it("remembers patterns per table, dropping what doesn't fit", () => {
    expect(patternsKey("c1", "public", "people")).toBe("c1|public.people");
    expect(parseStoredPatterns({ "c1|public.people": { age: "20..60", bad: 3 }, other: "x", empty: {} })).toEqual({ "c1|public.people": { age: "20..60" } });
    expect(parseStoredPatterns(null)).toEqual({});
  });

  it("uses the user's pattern over the column's default, and keeps an emptied one empty", () => {
    const age = col("age", { pattern: "18..90", pattern_hint: "18..90" });
    expect(patternOf(age, {})).toBe("18..90");
    expect(patternOf(age, { age: "30..40" })).toBe("30..40");
    expect(patternOf(age, { age: "" })).toBe("");
  });
});
