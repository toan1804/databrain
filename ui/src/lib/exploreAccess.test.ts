import { describe, expect, it } from "vitest";
import type { ConnectorKind } from "./bindings/ConnectorKind";
import { notPartitioned, partitionsSql, PARTITIONS_MAX, permissionsSql, unpartitioned } from "./exploreAccess";

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
    expect(notPartitioned("[INVALID_PARTITION_OPERATION.PARTITION_MANAGEMENT_IS_UNSUPPORTED] Table t does not support partition management")).toBe(true);
    expect(notPartitioned("PERMISSION_DENIED")).toBe(false);
    expect((partitionsSql("postgres", "s", "t", "table", undefined, true) as { unavailable: string }).unavailable).toContain("Redshift");
    // What each database answers for a table without partitions.
    const one = (columns: string[], row: (string | null)[]) => ({ columns, rows: [row], duration_ms: 1 });
    expect(unpartitioned("bigquery", one(["name", "row_count"], [null, "10"]))).toBe(true);
    expect(unpartitioned("bigquery", one(["name", "row_count"], ["20260101", "10"]))).toBe(false);
    expect(unpartitioned("clickhouse", one(["name", "row_count"], ["tuple()", "10"]))).toBe(true);
    expect(unpartitioned("mssql", one(["name", "bound", "row_count"], ["1", null, "10"]))).toBe(true);
    expect(unpartitioned("postgres", { columns: ["name"], rows: [], duration_ms: 1 })).toBe(true);
    expect(unpartitioned("postgres", one(["name"], ["orders_2026"]))).toBe(false);
  });

  it("reads who may do what on the table, table and column grants, saying why where there are none", () => {
    expect(sql(permissionsSql("postgres", "sales", "orders", "table"))).toMatch(/aclexplode\(coalesce\(c\.relacl, acldefault\('r', c\.relowner\)\)\)[\s\S]*att\.attacl/);
    // Redshift: no aclexplode or LATERAL, so information_schema at once.
    expect(sql(permissionsSql("postgres", "sales", "orders", "table", undefined, true))).toContain("information_schema.table_privileges");
    expect(sql(permissionsSql("mysql", "shop", "orders", "table"))).toMatch(/TABLE_PRIVILEGES[\s\S]*COLUMN_PRIVILEGES[\s\S]*SCHEMA_PRIVILEGES/);
    expect(sql(permissionsSql("mssql", "Sales.dbo", "Orders", "table"))).toMatch(/\[Sales\]\.sys\.database_permissions[\s\S]*pe\.class = 3/);
    expect(sql(permissionsSql("oracle", "APP", "ORDERS", "table"))).toMatch(/all_tab_privs[\s\S]*all_col_privs/);
    expect(sql(permissionsSql("snowflake", "DB.S", "V", "view"))).toBe("SHOW GRANTS ON VIEW DB.S.V");
    expect(sql(permissionsSql("snowflake", "DB.S", "X", "foreign_table"))).toBe("SHOW GRANTS ON EXTERNAL TABLE DB.S.X");
    expect(sql(permissionsSql("databricks", "main.s", "t", "table"))).toBe("SHOW GRANTS ON TABLE main.s.t");
    expect(sql(permissionsSql("clickhouse", "db", "t", "table"))).toContain("system.grants");
    for (const [k, why] of [["bigquery", "IAM"], ["sqlite", "no users"], ["duckdb", "no users"], ["mongodb", "database"]] as [ConnectorKind, string][]) {
      expect((permissionsSql(k, "s", "t", "table") as { unavailable: string }).unavailable).toContain(why);
    }
  });
});
