import { describe, expect, it } from "vitest";
import { addPair, changePair, pairColumns, pairLabels, parseMapping, removePair, type PairEdits, compareOptions, DEFAULT_COMPARE_FORM, parseTolerance, shortType, changedIn, diffLayout, diffView } from "./compare";

describe("compare: column matching", () => {
  it("matches equal names, then names that differ only in case", () => {
    const r = pairColumns(["ID", "test", "val_dt", "x"], ["id", "Test", "value_date", "y"], []);
    expect(r.pairs).toEqual([
      { before: "ID", after: "id", label: "id", how: "case" },
      { before: "test", after: "Test", label: "Test", how: "case" },
    ]);
    expect(r.onlyBefore).toEqual(["val_dt", "x"]);
    expect(r.onlyAfter).toEqual(["value_date", "y"]);
  });

  it("prefers exact names over case-insensitive ones", () => {
    const r = pairColumns(["TEST", "test"], ["test"], []);
    expect(r.pairs).toEqual([{ before: "test", after: "test", label: "test", how: "same" }]);
    expect(r.onlyBefore).toEqual(["TEST"]);
  });

  it("adds pairs by hand and ignores invalid ones", () => {
    const r = pairColumns(["val_dt", "amount"], ["value_date", "amount"], [["val_dt", "value_date"], ["nope", "amount"]]);
    expect(r.pairs.map((p) => `${p.before}>${p.after}:${p.how}`)).toEqual(["val_dt>value_date:manual", "amount>amount:same"]);
    expect(r.onlyBefore).toEqual([]);
  });

  it("adds a pair next to the ones compared, never in place of them", () => {
    const cols = ["id", "debit_amt", "credit_amt"];
    const st = addPair({ manual: [], excluded: [] }, "debit_amt", "credit_amt");
    const r = pairColumns(cols, cols, st.manual, st.excluded);
    expect(r.pairs.map((p) => `${p.before}>${p.after}=${p.label}`)).toEqual([
      "id>id=id",
      "debit_amt>debit_amt=debit_amt",
      "credit_amt>credit_amt=credit_amt",
      "debit_amt>credit_amt=debit_amt → credit_amt",
    ]);
    // The same pair again changes nothing.
    expect(addPair(st, "debit_amt", "credit_amt")).toBe(st);
  });

  it("removes and changes single pairs", () => {
    const before = ["id", "a", "b", "c"];
    const after = ["id", "a1", "b2", "c"];
    let st: PairEdits = addPair(addPair({ manual: [], excluded: [] }, "a", "a1"), "b", "b2");
    let r = pairColumns(before, after, st.manual, st.excluded);
    expect(r.pairs.map((p) => `${p.before}>${p.after}:${p.how}`)).toEqual(["id>id:same", "a>a1:manual", "b>b2:manual", "c>c:same"]);
    // Removing a matched pair leaves it out; removing an added one drops it.
    st = removePair(removePair(st, r.pairs[3]), r.pairs[2]);
    r = pairColumns(before, after, st.manual, st.excluded);
    expect(r.pairs.map((p) => p.after)).toEqual(["id", "a1"]);
    expect(r.onlyAfter).toEqual(["b2", "c"]);
    expect(r.onlyBefore).toEqual(["b", "c"]);
    // Changing a matched pair's before column.
    st = changePair(st, r.pairs[0], "a");
    r = pairColumns(before, after, st.manual, st.excluded);
    expect(r.pairs.map((p) => `${p.before}>${p.after}=${p.label}`)).toEqual(["a>id=id", "a>a1=a1"]);
  });

  it("labels pairs as the backend does", () => {
    expect(pairLabels([["a", "x"], ["x", "x"], ["b", "y"]])).toEqual(["a → x", "x", "y"]);
  });

  it("parses typed pairs", () => {
    const r = parseMapping("r1.a = r2.a1, B -> b2\nc→nope; x", ["a", "b", "c"], ["a1", "b2"]);
    expect(r.pairs).toEqual([["a", "a1"], ["b", "b2"]]);
    expect(r.errors).toEqual(["nope is not a column of the after output", '"x": write it as before = after']);
  });
});

describe("how values are compared", () => {
  it("names Arrow types the way people read them", () => {
    expect(shortType("Timestamp(µs)")).toBe("timestamp");
    expect(shortType('Timestamp(Microsecond, Some("UTC"))')).toBe("timestamptz");
    expect(shortType('Timestamp(µs, "UTC")')).toBe("timestamptz");
    expect(shortType("Timestamp(µs)")).toBe("timestamp");
    expect(shortType("Utf8")).toBe("text");
    expect(shortType("LargeUtf8")).toBe("text");
    expect(shortType("Decimal128(10, 2)")).toBe("decimal(10,2)");
    expect(shortType("Int32")).toBe("int32");
    expect(shortType("Float64")).toBe("double");
    expect(shortType("Date32")).toBe("date");
    expect(shortType("Boolean")).toBe("boolean");
  });

  it("reads the tolerance as typed", () => {
    expect(parseTolerance("")).toBeNull();
    expect(parseTolerance(" 0.01 ")).toBe(0.01);
    expect(parseTolerance("0,5")).toBe(0.5);
    expect(parseTolerance("0")).toBeNaN();
    expect(parseTolerance("-1")).toBeNaN();
    expect(parseTolerance("abc")).toBeNaN();
  });

  it("sends options and only the kinds of compared columns", () => {
    const f = { ...DEFAULT_COMPARE_FORM, ignoreCase: true, tolerance: "0.01", kinds: { d: "date" as const, gone: "exact" as const } };
    expect(compareOptions(f, ["d", "amt"])).toEqual({ trim: true, ignore_case: true, empty_is_null: false, tolerance: 0.01, kinds: [["d", "date"]] });
    expect(compareOptions({ ...DEFAULT_COMPARE_FORM, tolerance: "x" }, []).tolerance).toBeNull();
  });
});

describe("the compare view", () => {
  it("reads which columns changed, only for known names", () => {
    expect([...changedIn("amt, code", ["amt", "code", "id"])].sort()).toEqual(["amt", "code"]);
    expect([...changedIn("amt_total", ["amt", "amt_total"])]).toEqual(["amt_total"]);
    expect(changedIn(null, ["amt"]).size).toBe(0);
  });

  it("finds a diff result's columns and filters it", () => {
    const l = diffLayout(["_change", "id", "amt", "amt (before)", "code", "code (before)", "_changed"], ["amt", "code"]);
    expect(l).toEqual({ change: 0, changed: 6, value: { amt: 2, code: 4 }, before: { amt: 3, code: 5 } });
    expect(diffView(l, "changed", "amt").filters).toEqual([
      { column: 0, op: "equals", value: "changed" },
      { column: 6, op: "contains", value: "amt" },
    ]);
    expect(diffView(l, "all", null).filters).toEqual([]);
    // Without keys there is no _changed column.
    const k = diffLayout(["_change", "id", "amt"], ["id", "amt"]);
    expect(diffView(k, "added", "amt").filters).toEqual([{ column: 0, op: "equals", value: "added" }]);
  });
});
