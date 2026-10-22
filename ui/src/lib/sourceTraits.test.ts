import { describe, expect, it } from "vitest";
import { engineName, setSourceTraits, traitsOf, type SourceTraits } from "./sourceTraits";
import { qualifiedName, quoteIdent, selectTopSql } from "./util";
import { isThreeLevel, topLevelNoun } from "./catalog";
import { formatSql } from "./formatSql";
import type { ConnectorKind } from "./types";

describe("source traits", () => {
  it("built-in copy covers every current kind", () => {
    for (const k of ["sqlite", "postgres", "mysql", "mssql", "oracle", "snowflake", "databricks", "bigquery", "duckdb"]) {
      expect(traitsOf(k).name).not.toBe("this database");
    }
    expect(engineName("mssql")).toBe("SQL Server");
  });

  it("a new kind from the backend works without UI changes", () => {
    const mongo: SourceTraits = { ...traitsOf("postgres"), name: "MongoDB", language: "mongo_json", quote: "double", levels: "three", catalog_noun: "database", row_limit: "limit", load: null };
    setSourceTraits([{ kind: "mongodb" as ConnectorKind, traits: mongo }]);
    const k = "mongodb" as ConnectorKind;
    expect(engineName(k)).toBe("MongoDB");
    expect(isThreeLevel({ kind: k })).toBe(true);
    expect(topLevelNoun({ kind: k }, true)).toBe("databases");
    expect(qualifiedName(k, "shop.sales", "Orders")).toBe('shop.sales."Orders"');
    expect(quoteIdent(k, "a b")).toBe('"a b"');
    expect(() => formatSql("db.orders.find({})", k)).toThrow(/Format works on SQL/);
    // Unknown kinds fall back to standard SQL.
    expect(selectTopSql("nothing" as ConnectorKind, "s", "t")).toBe("SELECT *\nFROM s.t\nLIMIT 100;");
  });
});
