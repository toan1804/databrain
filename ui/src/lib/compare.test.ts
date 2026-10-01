import { describe, expect, it } from "vitest";
import { pairColumns } from "./compare";

describe("compare: column matching", () => {
  it("matches equal names, then names that differ only in case", () => {
    const r = pairColumns(["ID", "test", "val_dt", "x"], ["id", "Test", "value_date", "y"], []);
    expect(r.pairs).toEqual([
      { before: "ID", after: "id", how: "case" },
      { before: "test", after: "Test", how: "case" },
    ]);
    expect(r.onlyBefore).toEqual(["val_dt", "x"]);
    expect(r.onlyAfter).toEqual(["value_date", "y"]);
  });

  it("prefers exact names over case-insensitive ones", () => {
    const r = pairColumns(["TEST", "test"], ["test"], []);
    expect(r.pairs).toEqual([{ before: "test", after: "test", how: "same" }]);
    expect(r.onlyBefore).toEqual(["TEST"]);
  });

  it("applies manual pairs first and ignores invalid ones", () => {
    const r = pairColumns(["val_dt", "amount"], ["value_date", "amount"], [["val_dt", "value_date"], ["nope", "amount"]]);
    expect(r.pairs).toEqual([
      { before: "val_dt", after: "value_date", how: "manual" },
      { before: "amount", after: "amount", how: "same" },
    ]);
    expect(r.onlyBefore).toEqual([]);
  });
});
