import { describe, expect, it } from "vitest";
import { pairColumns, parseMapping, setPair } from "./compare";

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

  it("matches several columns by hand, overrides and leaves out automatic matches", () => {
    const before = ["id", "a", "b", "c"];
    const after = ["id", "a1", "b2", "c"];
    let st = { manual: [] as [string, string][], excluded: [] as string[] };
    st = setPair(st.manual, st.excluded, "a1", "a");
    st = setPair(st.manual, st.excluded, "b2", "b");
    let r = pairColumns(before, after, st.manual, st.excluded);
    expect(r.pairs.map((p) => `${p.before}>${p.after}:${p.how}`)).toEqual(["id>id:same", "a>a1:manual", "b>b2:manual", "c>c:same"]);
    st = setPair(st.manual, st.excluded, "c", null);
    r = pairColumns(before, after, st.manual, st.excluded);
    expect(r.pairs.map((p) => p.after)).toEqual(["id", "a1", "b2"]);
    expect(r.onlyAfter).toEqual(["c"]);
    expect(r.onlyBefore).toEqual(["c"]);
    // Moving a before column to another row frees its old row.
    st = setPair(st.manual, st.excluded, "c", "a");
    r = pairColumns(before, after, st.manual, st.excluded);
    expect(r.pairs.map((p) => `${p.before}>${p.after}`)).toEqual(["id>id", "b>b2", "a>c"]);
    expect(r.onlyAfter).toEqual(["a1"]);
  });

  it("parses typed pairs", () => {
    const r = parseMapping("r1.a = r2.a1, B -> b2\nc→nope; x", ["a", "b", "c"], ["a1", "b2"]);
    expect(r.pairs).toEqual([["a", "a1"], ["b", "b2"]]);
    expect(r.errors).toEqual(["nope is not a column of the after output", '"x": write it as before = after']);
  });
});
