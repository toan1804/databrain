import { describe, expect, it } from "vitest";
import { installPercent } from "../components/OracleClient";

describe("instant client install progress", () => {
  it("computes the percentage", () => {
    expect(installPercent(null)).toBeNull();
    expect(installPercent({ phase: "downloading", received: 57_409_586, total: 114_819_172 })).toBe(50);
    expect(installPercent({ phase: "downloading", received: 10, total: null })).toBeNull();
    expect(installPercent({ phase: "installing" })).toBe(100);
  });
});
