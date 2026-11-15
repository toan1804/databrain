import { describe, expect, it } from "vitest";
import { isOneCell } from "./ResultGrid";

describe("what a copy counts as one value", () => {
  it("one cell of one row; a whole row only when the result has one column", () => {
    expect(isOneCell(1, [2], 5)).toBe(true);
    expect(isOneCell(1, [], 1)).toBe(true);
    // Several cells: copied spreadsheet-safe.
    expect(isOneCell(1, [], 5)).toBe(false);
    expect(isOneCell(1, [1, 2], 5)).toBe(false);
    expect(isOneCell(2, [2], 5)).toBe(false);
  });
});
