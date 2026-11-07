import { describe, expect, it } from "vitest";
import type { LearnedJoinView } from "../../lib/types";
import { joinCondition, joinUsage } from "./LearnedSection";

const base: LearnedJoinView = {
  id: 1,
  left_table: "sales.order_items",
  left_columns: ["order_id", "line_no"],
  right_table: "sales.orders",
  right_columns: ["id", "line_no"],
  status: "learned",
  declared: false,
  queries: 3,
  days: 1,
  ai_queries: 0,
  kinds: { inner: 1, right: 2 },
  first_seen: 0,
  last_seen: null,
  shown: true,
  hidden_reason: null,
};

describe("learned joins", () => {
  it("renders a composite key as one condition", () => {
    expect(joinCondition(base)).toBe("sales.order_items.order_id = sales.orders.id AND sales.order_items.line_no = sales.orders.line_no");
  });

  it("summarizes use, the usual join kind and the AI's own queries", () => {
    expect(joinUsage(base)).toBe("3 queries · 1 day · usually RIGHT JOIN");
    const now = 10 * 86_400_000;
    expect(joinUsage({ ...base, queries: 1, days: 2, kinds: { inner: 1 }, ai_queries: 4, last_seen: now - 2 * 86_400_000 }, now)).toMatch(
      /^1 query · 2 days · last .+ · AI: 4$/,
    );
  });
});
