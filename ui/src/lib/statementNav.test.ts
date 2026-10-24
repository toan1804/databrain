import { describe, expect, it } from "vitest";
import { statementTarget } from "./statementNav";

describe("statement navigation", () => {
  const spans = [
    { start: 0, end: 8 },
    { start: 10, end: 18 },
    { start: 25, end: 30 },
  ];
  it("moves to the next statement's start", () => {
    expect(statementTarget(spans, 0, 1)).toBe(10);
    expect(statementTarget(spans, 12, 1)).toBe(25);
    expect(statementTarget(spans, 9, 1)).toBe(10); // between statements
    expect(statementTarget(spans, 26, 1)).toBeNull();
  });
  it("moves to the previous statement's start, or this one's from inside it", () => {
    expect(statementTarget(spans, 25, -1)).toBe(10);
    expect(statementTarget(spans, 28, -1)).toBe(25);
    expect(statementTarget(spans, 0, -1)).toBeNull();
    expect(statementTarget([], 3, 1)).toBeNull();
  });
});
