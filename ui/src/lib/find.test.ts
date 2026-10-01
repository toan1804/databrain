import { describe, expect, it } from "vitest";
import { planFind } from "./find";

const cols = ["order_id", "Country", "amount", "country_code"];

describe("find in results", () => {
  it("searches all values and lists matching column names", () => {
    expect(planFind("count", cols, null)).toEqual({ term: "count", columns: null, nameMatches: [1, 3] });
    expect(planFind("  ", cols, null)).toEqual({ term: "", columns: null, nameMatches: [] });
  });

  it("limits to the picked column", () => {
    expect(planFind("viet", cols, 1)).toEqual({ term: "viet", columns: [1], nameMatches: [] });
  });

  it("understands column: value and column=value", () => {
    expect(planFind("country: Viet Nam", cols, null)).toEqual({ term: "Viet Nam", columns: [1], nameMatches: [] });
    expect(planFind("AMOUNT=12", cols, null)).toEqual({ term: "12", columns: [2], nameMatches: [] });
    // Not a column name: the whole text is the value (e.g. a time "10:30").
    expect(planFind("10:30", cols, null)).toEqual({ term: "10:30", columns: null, nameMatches: [] });
  });
});
