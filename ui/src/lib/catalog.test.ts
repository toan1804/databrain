import { describe, expect, it } from "vitest";
import { columnList, schemaPath, tablePath, cachedHits, groupSchemas, matchRange, objectMatches, rankHits, revealKeys, schemaLabel, splitSchema, treeKey } from "./catalog";
import type { DbObject, SchemaInfo } from "./types";

const sch = (catalog: string | null, schema: string, is_default = false): SchemaInfo =>
  catalog ? { name: `${catalog}.${schema}`, catalog, is_default } : { name: schema, is_default };
const obj = (schema: string, name: string, kind: DbObject["kind"] = "table"): DbObject => ({ schema, name, kind });

describe("catalog tree", () => {
  it("groups three-level schemas under catalogs, default first", () => {
    const groups = groupSchemas([
      sch("main", "sales", true),
      sch("hive_metastore", "default"),
      sch("main", "default"),
      sch("main", "analytics"),
      sch("samples", "nyctaxi"),
    ])!;
    expect(groups.map((g) => g.name)).toEqual(["main", "hive_metastore", "samples"]);
    expect(groups[0].isDefault).toBe(true);
    expect(groups[0].schemas.map(schemaLabel)).toEqual(["sales", "analytics", "default"]);
  });

  it("keeps two-level engines flat", () => {
    expect(groupSchemas([sch(null, "public", true), sch(null, "audit")])).toBeNull();
    expect(schemaLabel(sch(null, "public"))).toBe("public");
  });

  it("splits schema ids for display", () => {
    expect(splitSchema("databricks", "main.sales")).toEqual({ catalog: "main", schema: "sales" });
    expect(splitSchema("postgres", "my.schema")).toEqual({ schema: "my.schema" });
    expect(splitSchema("snowflake", "DB.PUBLIC", [sch("DB", "PUBLIC")])).toEqual({ catalog: "DB", schema: "PUBLIC" });
  });

  it("reveal opens connection, catalog, schema and group", () => {
    const keys = revealKeys("c1", obj("main.sales", "v_orders", "view"), "main");
    expect(keys).toEqual(expect.arrayContaining([treeKey.conn("c1"), treeKey.catalog("c1", "main"), treeKey.schema("c1", "main.sales"), treeKey.group("c1", "main.sales", "Views")]));
  });
});

describe("catalog search", () => {
  it("matches names, or schema-qualified paths", () => {
    expect(objectMatches("ORD", "main.sales", "orders")).toBe(true);
    expect(objectMatches("sales.ord", "main.sales", "orders")).toBe(true);
    expect(objectMatches("main.sales.orders", "main.sales", "orders")).toBe(true);
    expect(objectMatches("crm.ord", "main.sales", "orders")).toBe(false);
    expect(objectMatches(" ", "s", "orders")).toBe(false);
  });

  it("ranks exact, prefix, then shorter names and drops routines/duplicates", () => {
    const hits = rankHits("orders", [
      obj("a", "big_orders"),
      obj("a", "orders_2024"),
      obj("b", "orders"),
      obj("a", "orders"),
      obj("a", "orders"),
      obj("a", "orders_fn", "function"),
    ]);
    expect(hits.map((o) => `${o.schema}.${o.name}`)).toEqual(["a.orders", "b.orders", "a.orders_2024", "a.big_orders"]);
    expect(rankHits("o", [obj("a", "x1o"), obj("a", "o2"), obj("a", "o")], 2).map((o) => o.name)).toEqual(["o", "o2"]);
  });

  it("finds cached explorer objects of one connection", () => {
    const objects = { "c1|main.sales": [obj("main.sales", "orders"), obj("main.sales", "users")], "c2|x": [obj("x", "orders")] };
    expect(cachedHits("ord", "c1", objects).map((o) => o.schema)).toEqual(["main.sales"]);
  });

  it("highlights the name term", () => {
    expect(matchRange("sales.Ord", "big_orders")).toEqual([4, 7]);
    expect(matchRange("zz", "orders")).toBeNull();
  });
});

describe("copy names", () => {
  it("quotes paths per dialect", () => {
    expect(schemaPath("databricks", "main.sales")).toBe("main.sales");
    expect(schemaPath("databricks", "main.Sales Data")).toBe("main.`Sales Data`");
    expect(schemaPath("snowflake", "DB.PUBLIC")).toBe("DB.PUBLIC");
    expect(schemaPath("snowflake", "db.public")).toBe('"db"."public"');
    expect(schemaPath("postgres", "my schema")).toBe('"my schema"');
    expect(schemaPath("bigquery", "proj-1.ds")).toBe("`proj-1.ds`");
    expect(tablePath("databricks", "main.sales", "orders")).toBe("main.sales.orders");
    expect(tablePath("mssql", "dbo", "Order Items")).toBe("dbo.[Order Items]");
    expect(tablePath("duckdb", "memory.files", "sales")).toBe("files.sales");
  });

  it("builds column lists", () => {
    expect(columnList("postgres", ["id", "Name"])).toBe('id, "Name"');
    expect(columnList("mysql", ["a", "b", "c", "d", "e"], "o")).toBe("o.a,\n  o.b,\n  o.c,\n  o.d,\n  o.e");
  });
});
