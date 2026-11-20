import { describe, expect, it } from "vitest";
import type { ConnectorKind } from "./bindings/ConnectorKind";
import { notPartitioned, partitionsSql, PARTITIONS_MAX, permissionsSql } from "./exploreAccess";

const sql = (b: { sql: string } | { unavailable: string }) => {
  if (!("sql" in b)) throw new Error(`unavailable: ${b.unavailable}`);
  return b.sql;
};

describe("table explorer: partitions and permissions", () => {
  it("lists partitions from each catalog, the first thousand, names as string literals", () => {
    expect(sql(partitionsSql("postgres", "sales", "o'rders", "table"))).toMatch(/pg_inherits[\s\S]*p\.relname = 'o''rders'[\s\S]*LIMIT 1000/);
    // Oracle: bounded by position (HIGH_VALUE is a LONG: no subquery around it).
    const ora = sql(partitionsSql("oracle", "APP", "ORDERS", "table"));
    expect(ora).toContain(`partition_position <= ${PARTITIONS_MAX}`);
    expect(ora).not.toMatch(/rownum|FETCH FIRST/i);
    expect(sql(partitionsSql("mysql", "shop", "orders", "table"))).toMatch(/AS row_count[\s\S]*information_schema\.PARTITIONS/);
    expect(sql(partitionsSql("mssql", "Sales.dbo", "Orders", "table"))).toMatch(/FROM \[Sales\]\.sys\.partitions[\s\S]*sc\.name = N'dbo'/);
    expect(sql(partitionsSql("databricks", "main.sales", "orders", "table"))).toBe("SHOW PARTITIONS main.sales.orders");
    expect(sql(partitionsSql("bigquery", "proj.ds", "t", "table"))).toContain("`proj.ds.INFORMATION_SCHEMA.PARTITIONS`");
    expect(sql(partitionsSql("clickhouse", "db", "t", "table"))).toMatch(/system\.parts[\s\S]*GROUP BY partition/);
    // A row count never named `rows` (reserved in MySQL 8).
    for (const k of ["postgres", "mysql", "mssql", "oracle", "bigquery", "clickhouse"] as ConnectorKind[]) expect(sql(partitionsSql(k, "s.s", "t", "table"))).not.toMatch(/ AS rows\b/);
    for (const [k, why] of [["snowflake", "micro-partitions"], ["sqlite", "aren't partitioned"], ["mongodb", "MongoDB"]] as [ConnectorKind, string][]) {
      expect((partitionsSql(k, "s", "t", "table") as { unavailable: string }).unavailable).toContain(why);
    }
    expect((partitionsSql("postgres", "s", "v", "view") as { unavailable: string }).unavailable).toContain("view");
    expect((partitionsSql("duckdb", "results", "r", "table", "results") as { unavailable: string }).unavailable).toContain("result");
    expect(notPartitioned("[INVALID_PARTITION_OPERATION.PARTITION_SCHEMA_IS_EMPTY] Table main.s.t is not partitioned")).toBe(true);
    expect(notPartitioned("PERMISSION_DENIED")).toBe(false);
  });

  it("reads who may do what on the table, table and column grants, saying why where there are none", () => {
    const pg = permissionsSql("postgres", "sales", "orders", "table");
    expect(pg).toHaveLength(2);
    expect(sql(pg[0])).toMatch(/aclexplode\(coalesce\(c\.relacl, acldefault\('r', c\.relowner\)\)\)[\s\S]*att\.attacl/);
    expect(sql(pg[1])).toContain("information_schema.table_privileges");
    expect(sql(permissionsSql("mysql", "shop", "orders", "table")[0])).toMatch(/TABLE_PRIVILEGES[\s\S]*COLUMN_PRIVILEGES[\s\S]*SCHEMA_PRIVILEGES/);
    expect(sql(permissionsSql("mssql", "Sales.dbo", "Orders", "table")[0])).toMatch(/\[Sales\]\.sys\.database_permissions[\s\S]*pe\.class = 3/);
    expect(sql(permissionsSql("oracle", "APP", "ORDERS", "table")[0])).toMatch(/all_tab_privs[\s\S]*all_col_privs/);
    expect(sql(permissionsSql("snowflake", "DB.S", "V", "view")[0])).toBe("SHOW GRANTS ON VIEW DB.S.V");
    expect(sql(permissionsSql("databricks", "main.s", "t", "table")[0])).toBe("SHOW GRANTS ON TABLE main.s.t");
    expect(sql(permissionsSql("clickhouse", "db", "t", "table")[0])).toContain("system.grants");
    for (const [k, why] of [["bigquery", "IAM"], ["sqlite", "no users"], ["duckdb", "no users"], ["mongodb", "database"]] as [ConnectorKind, string][]) {
      expect((permissionsSql(k, "s", "t", "table")[0] as { unavailable: string }).unavailable).toContain(why);
    }
  });
});
