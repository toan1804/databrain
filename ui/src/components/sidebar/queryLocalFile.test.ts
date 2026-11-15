import { describe, expect, it } from "vitest";
import { leftOutMessage } from "./queryLocalFile";

describe("Excel cells left out of a read", () => {
  it("says what is read of each sheet, not only in the SQL's comment", () => {
    expect(leftOutMessage([{ name: "Sales", left_out: "reads A4:D30 of A1:KO5001" }])).toBe(
      "Sales reads A4:D30 of A1:KO5001: title rows above the table and cells far from it are left out (the SQL says so too).",
    );
    expect(leftOutMessage([{ name: "A", left_out: "reads A2:B3 of A1:B9" }, { name: "B", left_out: "reads C1:D4 of A1:D4" }])).toMatch(/^A reads A2:B3 of A1:B9; B reads C1:D4 of A1:D4:/);
  });
});
