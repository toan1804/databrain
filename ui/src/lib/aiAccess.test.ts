import { describe, expect, it } from "vitest";
import { formatPatterns, like, matchesTable, parsePatterns, tableAccess } from "./aiAccess";

describe("AI table access patterns", () => {
  it("parses quoted lists", () => {
    expect(parsePatterns("'catalog_read%', '%schema_can_read%'\n other ")).toEqual(["catalog_read%", "%schema_can_read%", "other"]);
    expect(formatPatterns(["a%", "b"])).toBe("'a%', 'b'");
  });

  it("matches like SQL LIKE, on full and shorter names", () => {
    expect(like("catalog_read%", "catalog_read_x.s.t")).toBe(true);
    expect(like("a_c", "abbc")).toBe(false);
    expect(matchesTable(["sales.%"], "prod.sales.orders")).toBe("sales.%");
    expect(matchesTable(["orders"], "prod.sales.orders")).toBe("orders");
    expect(matchesTable(["cannot_read%"], "can_read.s.t")).toBeNull();
  });

  it("deny wins, an allow list limits", () => {
    const allow = parsePatterns("'catalog_read%', '%schema_can_read%'");
    const deny = parsePatterns("'%secret%'");
    expect(tableAccess(allow, deny, "catalog_read1.sales.orders").ok).toBe(true);
    expect(tableAccess(allow, deny, "x.my_schema_can_read.t").ok).toBe(true);
    expect(tableAccess(allow, deny, "catalog_read1.hr.secret_pay")).toEqual({ ok: false, why: "matches '%secret%' (deny)" });
    expect(tableAccess(allow, deny, "other.sales.orders").ok).toBe(false);
    expect(tableAccess([], deny, "other.sales.orders").ok).toBe(true);
  });
});
