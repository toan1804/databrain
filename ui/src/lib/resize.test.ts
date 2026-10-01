import { describe, expect, it } from "vitest";
import { EDITOR_MIN, OUTPUT_MAX, clampHeight } from "./resize";

describe("cell resize", () => {
  it("clamps to the allowed range", () => {
    expect(clampHeight(10, EDITOR_MIN, 500)).toBe(EDITOR_MIN);
    expect(clampHeight(99999, 120, OUTPUT_MAX)).toBe(OUTPUT_MAX);
    expect(clampHeight(250.6, 120, 2000)).toBe(251);
  });
});
