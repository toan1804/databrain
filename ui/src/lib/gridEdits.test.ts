import { describe, expect, it } from "vitest";
import { addRow, editCount, emptyEdits, revertRows, setCell, setNewCell, toChanges, toggleDelete } from "./gridEdits";

describe("grid edits", () => {
  it("tracks changed cells, new rows and deletions as changes", () => {
    let e = emptyEdits();
    e = setCell(e, 7, ["1"], 1, "Ann", "Anna");
    e = setCell(e, 7, ["1"], 2, "30", null);
    e = addRow(e);
    e = setNewCell(e, 0, 1, "Dee");
    e = toggleDelete(e, [{ id: 9, key: ["3"] }]);
    expect(editCount(e)).toBe(4);
    expect(toChanges(e)).toEqual([
      { op: "update", key: ["1"], values: [{ column: 1, value: "Anna" }, { column: 2, value: null }] },
      { op: "insert", values: [{ column: 1, value: "Dee" }] },
      { op: "delete", key: ["3"] },
    ]);
  });
  it("typing the original value back removes the change", () => {
    let e = setCell(emptyEdits(), 1, ["1"], 1, "Ann", "Anna");
    e = setCell(e, 1, ["1"], 1, "Ann", "Ann");
    expect(editCount(e)).toBe(0);
  });
  it("deleting a row drops its cell changes; toggling again restores it; revert clears", () => {
    let e = setCell(emptyEdits(), 1, ["1"], 1, "a", "b");
    e = toggleDelete(e, [{ id: 1, key: ["1"] }]);
    expect(toChanges(e)).toEqual([{ op: "delete", key: ["1"] }]);
    e = toggleDelete(e, [{ id: 1, key: ["1"] }]);
    expect(editCount(e)).toBe(0);
    e = revertRows(setCell(e, 2, ["2"], 0, "x", "y"), [2]);
    expect(editCount(e)).toBe(0);
  });
});
