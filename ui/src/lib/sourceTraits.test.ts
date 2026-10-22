import { describe, expect, it } from "vitest";
import { engineName, setSourceTraits, traitsOf, type SourceTraits } from "./sourceTraits";
import { qualifiedName, quoteIdent, selectTopSql } from "./util";
import { groupLabel, isThreeLevel, topLevelNoun } from "./catalog";
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
    setSourceTraits([{ kind: "fakedb" as ConnectorKind, traits: mongo }]);
    const k = "fakedb" as ConnectorKind;
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

describe("MongoDB and Neo4j", () => {
  it("preview queries, folders and levels", () => {
    expect(selectTopSql("mongodb", "shop", "orders")).toBe('db.getSiblingDB("shop").getCollection("orders").find({}).limit(100)');
    expect(selectTopSql("neo4j", "Nodes", "Person", 5)).toBe("MATCH (n:`Person`)\nRETURN n\nLIMIT 5");
    expect(selectTopSql("neo4j", "Relationships", "ACTED_IN", 5)).toBe("MATCH (a)-[r:`ACTED_IN`]->(b)\nRETURN a, r, b\nLIMIT 5");
    expect(groupLabel("Tables", "mongodb")).toBe("Collections");
    expect(groupLabel("Views", "neo4j")).toBe("Relationship types");
    expect(groupLabel("Tables", "postgres")).toBe("Tables");
    expect(isThreeLevel({ kind: "mongodb" })).toBe(false);
    expect(traitsOf("mongodb").language).toBe("mongo_json");
    expect(traitsOf("neo4j").load).toBeNull();
  });
});
