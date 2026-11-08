import { describe, expect, it } from "vitest";
import { charDiff, describeChar, differsUnseen } from "./textDiff";

const marked = (cs: { ch: string; changed: boolean }[]) => cs.filter((c) => c.changed).map((c) => c.ch);

describe("text differences you can't see", () => {
  it("finds the characters that differ, keeping the rest", () => {
    const d = charDiff("ABC 123", "ABC 123");
    expect(marked(d.before)).toEqual([" "]);
    expect(marked(d.after)).toEqual([" "]);
    expect(d.after.find((c) => c.changed)?.name).toBe("U+00A0 NO-BREAK SPACE");
    // An insertion in the middle: only it is marked.
    const ins = charDiff("hello world", "hello​ world");
    expect(marked(ins.before)).toEqual([]);
    expect(marked(ins.after)).toEqual(["​"]);
  });

  it("draws spaces, tabs, new lines and invisible characters", () => {
    expect(describeChar(" ").shown).toBe("·");
    expect(describeChar("\t")).toMatchObject({ shown: "→", name: "U+0009 TAB", kind: "space" });
    expect(describeChar("\n").shown).toBe("↵");
    expect(describeChar("﻿")).toMatchObject({ shown: "BOM", kind: "hidden" });
    expect(describeChar("\u0001")).toMatchObject({ shown: "\\x01", kind: "hidden" });
    expect(describeChar("�").name).toContain("wrong encoding");
    expect(describeChar("a").kind).toBe("plain");
  });

  it("tells an accent written apart from a composed one", () => {
    const d = charDiff("Café", "Café");
    expect(marked(d.before)).toEqual(["é"]);
    expect(marked(d.after)).toEqual(["e", "́"]);
    expect(describeChar("́")).toMatchObject({ shown: "◌́", kind: "other" });
  });

  it("says when values differ only in what can't be seen", () => {
    expect(differsUnseen("ABC 123", "ABC 123")).toBe(true);
    expect(differsUnseen("a  b", "a\tb")).toBe(true);
    expect(differsUnseen("Café", "Café")).toBe(true);
    expect(differsUnseen("x​", "x")).toBe(true);
    expect(differsUnseen("Zed", "zed")).toBe(false);
    expect(differsUnseen("same", "same")).toBe(false);
    expect(differsUnseen(null, "x")).toBe(false);
  });

  it("stays fast on long values (common start and end only)", () => {
    const a = "x".repeat(2000) + "a".repeat(1000) + "y".repeat(2000);
    const b = "x".repeat(2000) + "b".repeat(1000) + "y".repeat(2000);
    const d = charDiff(a, b);
    expect(marked(d.before).length).toBe(1000);
    expect(marked(d.after).length).toBe(1000);
  });
});
