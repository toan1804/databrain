import { describe, expect, it } from "vitest";
import { sampleSql, shortCount } from "./util";

describe("the table explorer's sample rows", () => {
  it("take the first N rows in each database's way, with a condition when given", () => {
    expect(sampleSql("postgres", "public", "orders", 100)).toBe('SELECT *\nFROM public.orders\nLIMIT 100;');
    expect(sampleSql("postgres", "public", "orders", 100, "status = 'paid';  ")).toBe('SELECT *\nFROM public.orders\nWHERE status = \'paid\'\nLIMIT 100;');
    expect(sampleSql("mssql", "dbo", "orders", 1000, "total > 5")).toBe("SELECT TOP 1000 *\nFROM dbo.orders\nWHERE total > 5;");
    expect(sampleSql("oracle", "SALES", "ORDERS", 100, "rownum < 5")).toBe('SELECT *\nFROM SALES.ORDERS\nWHERE rownum < 5\nFETCH FIRST 100 ROWS ONLY');
    // A blank condition is none.
    expect(sampleSql("postgres", "public", "orders", 10, "   ")).toBe('SELECT *\nFROM public.orders\nLIMIT 10;');
  });

  it("leave MongoDB and Neo4j to their own preview (no SQL condition)", () => {
    expect(sampleSql("mongodb", "shop", "orders", 100, "x = 1")).toBe('db.getSiblingDB("shop").getCollection("orders").find({}).limit(100)');
  });

  it("show row estimates short", () => {
    expect([shortCount(980), shortCount(50_000), shortCount(1_234_567), shortCount(15_000_000), shortCount(2_500_000_000)]).toEqual(["980", "50K", "1.2M", "15M", "2.5B"]);
  });
});
