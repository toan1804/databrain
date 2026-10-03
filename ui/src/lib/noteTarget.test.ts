import { describe, expect, it } from "vitest";
import { applySuggestion, currentSegment, findTable, suggest, type TargetTable } from "./noteTarget";

const tables: TargetTable[] = [
  { schema: "sales", name: "orders", columns: ["order_id", "customer_id", "status"] },
  { schema: "sales", name: "order_items" },
  { schema: "crm", name: "customers", columns: ["customer_id", "name"] },
  { schema: "main.dw", name: "orders_daily" },
];

describe("note target completion", () => {
  it("finds the table path under the cursor between separators", () => {
    const t = "sales.orders & crm.cus and x";
    expect(currentSegment(t, 22)).toEqual({ from: 15, to: 22, word: "crm.cus" });
    expect(currentSegment(t, 5).word).toBe("sales");
    expect(currentSegment(t, t.length)).toEqual({ from: 27, to: 28, word: "x" });
    expect(currentSegment("a, b", 4).word).toBe("b");
    // "and" inside a word is not a separator.
    expect(currentSegment("brand", 5).word).toBe("brand");
  });

  it("suggests tables by name or schema, prefix matches first", () => {
    expect(suggest("ord", tables).map((s) => s.insert)).toEqual(["sales.orders", "sales.order_items", "main.dw.orders_daily"]);
    expect(suggest("crm.", tables).map((s) => s.insert)).toEqual(["crm.customers"]);
    expect(suggest("tom", tables).map((s) => s.insert)).toEqual(["crm.customers"]);
  });

  it("suggests columns after table.", () => {
    expect(suggest("orders.cu", tables).map((s) => s.insert)).toEqual(["sales.orders.customer_id"]);
    expect(suggest("sales.orders.", tables).map((s) => s.label)).toEqual(["order_id", "customer_id", "status"]);
    expect(findTable(tables, "dw.orders_daily")?.schema).toBe("main.dw");
  });

  it("replaces only the current segment", () => {
    const t = "sales.orders & cus or crm.customers";
    const seg = currentSegment(t, 18);
    expect(applySuggestion(t, seg, "crm.customers")).toEqual({ text: "sales.orders & crm.customers or crm.customers", cursor: 28 });
  });
});
