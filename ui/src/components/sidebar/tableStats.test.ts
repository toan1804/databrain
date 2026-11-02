import { describe, expect, it } from "vitest";
import { statsText } from "./ObjectNodes";

describe("table rows and size", () => {
  it("shows what the database knows, short in the row and in full on hover", () => {
    expect(statsText(1_234_567, 356_515_840)).toEqual({ short: "1.2M · 340 MB", long: "≈ 1,234,567 rows · 340 MB (from the database's statistics)" });
    expect(statsText(null, 2048).short).toBe("2.0 KB");
    expect(statsText(42, null).short).toBe("42");
    expect(statsText(-1, 0)).toEqual({});
  });
});
