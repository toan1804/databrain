import { describe, expect, it } from "vitest";
import { formatSql, formatterDialect } from "./formatSql";

describe("formatSql", () => {
  it("formats per dialect with upper-case keywords", () => {
    expect(formatSql("select a, b from t where x = 1", "postgres")).toBe("SELECT\n  a,\n  b\nFROM\n  t\nWHERE\n  x = 1");
    expect(formatSql("select top 5 * from [dbo].[t]", "mssql")).toContain("[dbo].[t]");
    expect(formatSql("select `a` from `t`", "mysql")).toContain("`a`");
    expect(formatterDialect("databricks")).toBe("spark");
    expect(formatterDialect(undefined)).toBe("sql");
  });

  it("keeps the selection's surrounding whitespace and indentation", () => {
    const out = formatSql("\n    select a from t\n", "postgres");
    expect(out).toBe("\n    SELECT\n      a\n    FROM\n      t\n");
  });

  it("formats several statements and leaves blank input alone", () => {
    expect(formatSql("select 1; select 2;", "sqlite")).toBe("SELECT\n  1;\n\nSELECT\n  2;");
    expect(formatSql("   ", "postgres")).toBe("   ");
  });

  it("throws on SQL it cannot parse", () => {
    expect(() => formatSql("select 'unterminated", undefined)).toThrow();
  });
});
