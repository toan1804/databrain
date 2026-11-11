import { describe, expect, it } from "vitest";
import type { ConnectorKind } from "./types";
import { formatSql, formatterDialect, protectParams } from "./formatSql";

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
  // Spans as the backend's sql_parameters finds them (here: every `:name`
  // that isn't part of a `::` cast).
  const spans = (sql: string) => [...sql.matchAll(/(?<!:):[A-Za-z_]\w*/g)].map((m) => ({ from: m.index, to: m.index + m[0].length }));

  it("keeps :name parameters as written in every dialect", () => {
    const sql = "select a, b from t where d >= :start_day and e = :id and f in (:ids) limit :n";
    const kinds: ConnectorKind[] = ["postgres", "mysql", "sqlite", "mssql", "oracle", "bigquery", "snowflake", "duckdb", "clickhouse", "databricks"];
    for (const k of kinds) {
      const out = formatSql(sql, k, { params: spans(sql) });
      for (const p of [">= :start_day", "= :id", "(:ids)", ":n"]) expect(out, `${k}: ${out}`).toContain(p);
      expect(out, k).not.toMatch(/: [a-z]/);
      expect(out, k).toContain("WHERE");
    }
  });

  it("keeps casts, strings and the selection's indentation around parameters", () => {
    const sql = "  select a::text, ':not_a_param' as s from t where id = :id";
    const out = formatSql(sql, "postgres", { params: spans(sql).filter((p) => sql.slice(p.from, p.to) === ":id") });
    expect(out).toContain("a::text");
    expect(out).toContain("':not_a_param'");
    expect(out).toContain("id = :id");
    expect(out.startsWith("  SELECT")).toBe(true);
  });

  it("puts parameters back exactly, Oracle &name too", () => {
    const sql = "select * from emp where dept = &dept and id = :id";
    const out = formatSql(sql, "oracle", { params: [{ from: 31, to: 36 }, { from: 46, to: 49 }] });
    expect(out).toContain("dept = &dept");
    expect(out).toContain("id = :id");
    const p = protectParams("x dbparam :a", [{ from: 10, to: 12 }]);
    expect(p.text).not.toContain(":a");
    expect(p.restore(p.text)).toBe("x dbparam :a");
  });
});
