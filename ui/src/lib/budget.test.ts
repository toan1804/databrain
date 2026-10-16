import { describe, expect, it } from "vitest";
import { meterText } from "./budget";

describe("budget meters", () => {
  it("reads like the provider's own page", () => {
    expect(meterText({ label: "Credits", used: 6368.25, limit: 10000, remaining: 3631.75, unit: "credits" })).toBe("6,368 credits of 10,000 used (64%)");
    expect(meterText({ label: "Balance", used: null, limit: null, remaining: 110, unit: "CNY" })).toBe("110.00 CNY left");
    expect(meterText({ label: "This key", used: 1.25, limit: null, remaining: null, unit: "USD" })).toBe("1.25 USD spent (no limit)");
  });
});
