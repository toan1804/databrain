import { describe, expect, it } from "vitest";
import type { ConnectorKind } from "./bindings/ConnectorKind";
import { asOfSql, checksSql, columnClass, countSql, exploreKey, historyKind, historySql, indexUseSql, profileSql, PROFILE_MAX_COLUMNS, readAllReasons, readConstraints, readProfile, rowsWritten, sqlString, statisticsSql, takeExploreKeys, topValuesSql, triggersSql, type Built } from "./explore";

const sql = (b: { sql: string } | { unavailable: string }) => {
  if (!("sql" in b)) throw new Error(`unavailable: ${b.unavailable}`);
  return b.sql;
};

describe("table explorer: profile, statistics and counts", () => {
  it("classes columns by their type, as each database names it", () => {
    const cases: [ConnectorKind, string, string][] = [
      ["postgres", "integer", "number"],
      ["postgres", "numeric(12,2)", "number"],
      ["postgres", "double precision", "number"],
      ["postgres", "timestamp with time zone", "time"],
      ["postgres", "character varying(16)", "text"],
      ["postgres", "text", "text"],
      ["postgres", "uuid", "text"],
      ["postgres", "boolean", "text"],
      ["postgres", "json", "other"],
      ["postgres", "jsonb", "other"],
      ["postgres", "integer[]", "other"],
      ["postgres", "bytea", "other"],
      ["postgres", "interval", "other"],
      ["mysql", "int unsigned", "number"],
      ["mysql", "datetime", "time"],
      ["mysql", "longtext", "text"],
      ["mysql", "json", "other"],
      ["mysql", "blob", "other"],
      ["mssql", "nvarchar(50)", "text"],
      ["mssql", "text", "other"],
      ["mssql", "ntext", "other"],
      ["mssql", "datetime2", "time"],
      ["mssql", "bit", "text"],
      ["mssql", "xml", "other"],
      ["oracle", "NUMBER(10,0)", "number"],
      ["oracle", "VARCHAR2(100)", "text"],
      ["oracle", "DATE", "time"],
      ["oracle", "TIMESTAMP(6)", "time"],
      ["oracle", "CLOB", "other"],
      ["oracle", "LONG", "other"],
      ["oracle", "RAW(16)", "other"],
      ["snowflake", "NUMBER(38,0)", "number"],
      ["snowflake", "TIMESTAMP_NTZ(9)", "time"],
      ["snowflake", "VARIANT", "other"],
      ["bigquery", "INT64", "number"],
      ["bigquery", "FLOAT64", "number"],
      ["bigquery", "STRING", "text"],
      ["bigquery", "ARRAY<STRING>", "other"],
      ["bigquery", "STRUCT<a INT64>", "other"],
      ["databricks", "decimal(10,2)", "number"],
      ["databricks", "map<string,int>", "other"],
      ["clickhouse", "Nullable(Int32)", "number"],
      ["clickhouse", "LowCardinality(Nullable(String))", "text"],
      ["clickhouse", "DateTime64(3)", "time"],
      ["clickhouse", "Array(String)", "other"],
      ["duckdb", "HUGEINT", "number"],
      ["sqlite", "REAL", "number"],
    ];
    for (const [kind, type, want] of cases) expect([kind, type, columnClass(kind, type)]).toEqual([kind, type, want]);
  });

  it("profiles every column in one query: distinct where comparable, min and max of numbers and times", () => {
    const cols = [
      { name: "id", data_type: "bigint" },
      { name: "status", data_type: "varchar(16)" },
      { name: "payload", data_type: "jsonb" },
      { name: "created_at", data_type: "timestamp" },
    ];
    expect(sql(profileSql("postgres", "sales", "orders", cols))).toBe(
      [
        "SELECT",
        "  COUNT(*) AS total,",
        "  COUNT(id) AS n0,",
        "  COUNT(DISTINCT id) AS d0,",
        "  MIN(id) AS lo0,",
        "  MAX(id) AS hi0,",
        "  COUNT(status) AS n1,",
        "  COUNT(DISTINCT status) AS d1,",
        "  COUNT(payload) AS n2,",
        "  COUNT(created_at) AS n3,",
        "  COUNT(DISTINCT created_at) AS d3,",
        "  MIN(created_at) AS lo3,",
        "  MAX(created_at) AS hi3",
        "FROM sales.orders",
      ].join("\n"),
    );
    // Approximate where the database has it; names quoted as the database wants.
    expect(sql(profileSql("snowflake", "DB.SALES", "ORDERS", [{ name: "Status", data_type: "TEXT" }]))).toContain('APPROX_COUNT_DISTINCT("Status") AS d0');
    expect(sql(profileSql("clickhouse", "default", "t", [{ name: "x", data_type: "String" }]))).toContain("uniq(x) AS d0");
    expect(sql(profileSql("mssql", "dbo", "t", [{ name: "order", data_type: "int" }]))).toContain("COUNT(DISTINCT [order]) AS d0");
    // A DuckDB file is read as Query names it.
    expect(sql(profileSql("duckdb", "x.files", "f", [{ name: "a", data_type: "INTEGER" }], "read_csv('/tmp/f.csv')"))).toContain("FROM read_csv('/tmp/f.csv')");
    // Wide tables: the first columns only.
    const wide = Array.from({ length: PROFILE_MAX_COLUMNS + 5 }, (_, i) => ({ name: `c${i}`, data_type: "int" }));
    expect(sql(profileSql("postgres", "s", "t", wide))).not.toContain(`AS n${PROFILE_MAX_COLUMNS}`);
    // MongoDB and Neo4j say why not.
    expect(profileSql("mongodb", "db", "coll", cols)).toHaveProperty("unavailable");
    expect(profileSql("neo4j", "labels", "Person", cols)).toHaveProperty("unavailable");
  });

  it("limits top values as each database does", () => {
    expect(sql(topValuesSql("postgres", "s", "t", "status"))).toMatch(/GROUP BY status\nORDER BY n DESC\nLIMIT 10$/);
    expect(sql(topValuesSql("mssql", "dbo", "t", "status"))).toMatch(/^SELECT TOP 10 status AS value/);
    expect(sql(topValuesSql("oracle", "HR", "T", "STATUS"))).toMatch(/FETCH FIRST 10 ROWS ONLY$/);
    expect(sql(countSql("bigquery", "p.d", "t"))).toBe("SELECT COUNT(*) AS n\nFROM `p.d.t`");
  });

  it("reads statistics from each catalog, with names as string literals", () => {
    const kinds: ConnectorKind[] = ["postgres", "mysql", "mssql", "oracle", "snowflake", "bigquery", "databricks", "duckdb", "clickhouse"];
    for (const k of kinds) expect(sql(statisticsSql(k, k === "snowflake" || k === "bigquery" || k === "databricks" || k === "duckdb" ? "db.sch" : "sch", "t", "table"))).toMatch(/row_estimate|DESCRIBE DETAIL/);
    // A quote in a name stays inside its literal.
    expect(sql(statisticsSql("postgres", "s", "o'brien", "table"))).toContain("c.relname = 'o''brien'");
    expect(sql(statisticsSql("mysql", "s", "a\\'b", "table"))).toContain("t.TABLE_NAME = 'a\\\\\\'b'");
    expect(sqlString("bigquery", "it's")).toBe("'it\\'s'");
    // Three-level names: the database part goes to the catalog's own views.
    expect(sql(statisticsSql("snowflake", "SHOP.SALES", "ORDERS", "table"))).toContain("FROM SHOP.information_schema.tables\nWHERE table_schema = 'SALES' AND table_name = 'ORDERS'");
    expect(sql(statisticsSql("mssql", "Shop.dbo", "orders", "table"))).toContain("FROM [Shop].sys.objects o");
    expect(sql(statisticsSql("mssql", "dbo", "orders", "table"))).toContain("FROM sys.objects o");
    expect(sql(statisticsSql("bigquery", "proj.ds", "t", "table"))).toContain("FROM `proj.ds.__TABLES__`");
    expect(sql(statisticsSql("duckdb", "memory.main", "t", "table"))).toContain("database_name = 'memory' AND schema_name = 'main' AND table_name = 't'");
    // What has none says why.
    expect(statisticsSql("postgres", "s", "v", "view")).toEqual({ unavailable: expect.stringContaining("view") });
    expect(statisticsSql("sqlite", "main", "t", "table")).toEqual({ unavailable: expect.stringContaining("Count exactly") });
    expect(statisticsSql("duckdb", "results", "r", "table", "results")).toHaveProperty("unavailable");
    expect(statisticsSql("mongodb", "db", "c", "table")).toHaveProperty("unavailable");
  });

  it("reads a profile row back in any case", () => {
    const p = readProfile({ columns: ["TOTAL", "N0", "D0", "LO0", "HI0", "N1"], rows: [["10", "9", "3", "1", "7", "10"]], duration_ms: 1 }, 2);
    expect(p).toEqual({
      total: 10,
      columns: [
        { nonNull: 9, distinct: 3, lo: "1", hi: "7" },
        { nonNull: 10, distinct: null, lo: null, hi: null },
      ],
    });
  });

  describe("asking before a read of every row", () => {
    const base = { kind: "postgres" as ConnectorKind, prod: false, connName: "Shop", table: "orders", rowEstimate: 10, asked: false };

    it("runs a small table at once, and asks on production or past a million rows, until the tab has asked", () => {
      expect(readAllReasons(base)).toEqual([]);
      expect(readAllReasons({ ...base, prod: true })).toEqual(['"Shop" is a production connection.']);
      expect(readAllReasons({ ...base, rowEstimate: 5_000_000 })[0]).toMatch(/orders has about 5(\.0)?M rows/);
      expect(readAllReasons({ ...base, prod: true, rowEstimate: 5_000_000, asked: true })).toEqual([]);
    });

    it("asks when a server database doesn't say how many rows there are, not for a local file", () => {
      expect(readAllReasons({ ...base, rowEstimate: null })[0]).toMatch(/doesn't say how many rows orders has/);
      expect(readAllReasons({ ...base, kind: "sqlite", rowEstimate: null })).toEqual([]);
      expect(readAllReasons({ ...base, kind: "duckdb", rowEstimate: null })).toEqual([]);
    });

    it("asks about BigQuery's bill at every read that bills, even once the tab has asked", () => {
      const bq = { ...base, kind: "bigquery" as ConnectorKind, asked: true };
      const est = (bytes: number | null, over_limit: number | null = null) => ({ method: "dry run", bytes, over_limit, cache_hit: false, notes: [] });
      // A count BigQuery answers from metadata bills nothing: no question.
      expect(readAllReasons({ ...bq, estimate: est(0) })).toEqual([]);
      // The profile after it scans every column: asked, although the tab has asked before.
      expect(readAllReasons({ ...bq, estimate: est(5 * 1024 ** 4) })[0]).toMatch(/reads about 5(\.0)? TB/);
      expect(readAllReasons({ ...bq, estimate: est(10, 5) })).toHaveLength(2);
      expect(readAllReasons({ ...bq, estimate: null })[0]).toMatch(/couldn't be read/);
    });
  });

  it("notes each session key of a tab, to close them all with it", () => {
    expect(exploreKey("t9", "profile")).toBe("t9#profile");
    exploreKey("t9", "top:status");
    exploreKey("t9", "profile");
    exploreKey("t8", "count");
    expect(takeExploreKeys("t9").sort()).toEqual(["t9#profile", "t9#top:status"]);
    expect(takeExploreKeys("t9")).toEqual([]);
    expect(takeExploreKeys("t8")).toEqual(["t8#count"]);
  });
});

describe("table explorer, step 4: constraints, index use, history", () => {
  it("reads checks and triggers apart from each catalog, saying why where there are none", () => {
    const first = (b: Built[]) => sql(b[0]);
    for (const k of ["postgres", "mysql", "mssql", "oracle", "duckdb", "databricks"] as const) expect(checksSql(k, k === "duckdb" || k === "databricks" ? "db.sch" : "sch", "t")[0]).toHaveProperty("sql");
    for (const k of ["postgres", "mysql", "mssql", "oracle", "sqlite"] as const) expect(triggersSql(k, "sch", "t")).toHaveProperty("sql");
    expect(first(checksSql("postgres", "s", "o'b"))).toContain("c.relname = 'o''b'");
    // Unique keys are among the indexes, not listed again.
    expect(first(checksSql("postgres", "s", "t"))).toContain("con.contype IN ('c', 'x')");
    expect(first(checksSql("duckdb", "memory.main", "t"))).toContain("constraint_type = 'CHECK'");
    // MariaDB's form (checks named per table) first, then MySQL's.
    const my = checksSql("mysql", "shop", "t").map(sql);
    expect(my[0]).toContain("AND cc.TABLE_NAME = tc.TABLE_NAME");
    expect(my[1]).not.toContain("cc.TABLE_NAME");
    // Oracle: only system-named NOT NULL checks are left out; before 12.2, the names.
    const ora = checksSql("oracle", "HR", "T").map(sql);
    expect(ora[0]).toContain("NOT (generated = 'GENERATED NAME' AND search_condition_vc LIKE '% IS NOT NULL')");
    expect(ora[1]).not.toContain("search_condition_vc");
    expect(first(checksSql("mssql", "Shop.dbo", "t"))).toContain("FROM [Shop].sys.check_constraints cc");
    expect(sql(triggersSql("mssql", "dbo", "t"))).toContain("FROM sys.triggers tr");
    expect(sql(triggersSql("sqlite", "main", "t"))).toBe("SELECT 'trigger' AS what, name, sql AS definition\nFROM main.sqlite_master\nWHERE type = 'trigger' AND tbl_name = 't'");
    expect(checksSql("sqlite", "main", "t")[0]).toHaveProperty("unavailable");
    for (const k of ["snowflake", "bigquery", "clickhouse", "mongodb", "neo4j"] as const) {
      expect(checksSql(k, "s", "t")[0]).toHaveProperty("unavailable");
      expect(triggersSql(k, "s", "t")).toHaveProperty("unavailable");
    }
    expect(checksSql("duckdb", "results", "r", "results")[0]).toHaveProperty("unavailable");
    // Databricks: the Delta constraints among the table properties.
    expect(
      readConstraints("databricks", { columns: ["key", "value"], rows: [["delta.minReaderVersion", "1"], ["delta.constraints.positive", "amount > 0"]], duration_ms: 1 }),
    ).toEqual([{ what: "check", name: "positive", definition: "amount > 0" }]);
    expect(readConstraints("oracle", { columns: ["WHAT", "NAME", "DEFINITION"], rows: [["check", "CK", "x > 0"]], duration_ms: 1 })).toEqual([{ what: "check", name: "CK", definition: "x > 0" }]);
    expect(indexUseSql("postgres", "s", "t")).toHaveProperty("sql");
    expect(indexUseSql("mysql", "s", "t")).toHaveProperty("unavailable");
  });

  it("opens a table as it was: a Delta version, or a time (checked, so it can't carry SQL)", () => {
    expect(historyKind("databricks")).toBe("versions");
    expect(historyKind("snowflake")).toBe("as_of");
    expect(historyKind("postgres")).toBeNull();
    expect(sql(historySql("databricks", "cat.sch", "t"))).toBe("DESCRIBE HISTORY cat.sch.t LIMIT 50");
    expect(sql(asOfSql("databricks", "cat.sch", "t", { version: 7 }))).toBe("SELECT *\nFROM cat.sch.t VERSION AS OF 7\nLIMIT 100");
    expect(sql(asOfSql("databricks", "cat.sch", "t", { time: "2026-11-20T08:30" }))).toContain("TIMESTAMP AS OF '2026-11-20 08:30'");
    expect(sql(asOfSql("snowflake", "DB.S", "T", { time: "2026-11-20" }))).toContain("AT(TIMESTAMP => '2026-11-20'::TIMESTAMP_LTZ)");
    expect(sql(asOfSql("bigquery", "p.d", "t", { time: "2026-11-20 08:30:05" }))).toContain("FOR SYSTEM_TIME AS OF TIMESTAMP '2026-11-20 08:30:05'");
    expect(asOfSql("snowflake", "DB.S", "T", { time: "2026-11-20'; drop table t; --" })).toHaveProperty("unavailable");
    expect(asOfSql("snowflake", "DB.S", "T", { version: 1 })).toHaveProperty("unavailable");
    expect(asOfSql("postgres", "s", "t", { time: "2026-11-20" })).toHaveProperty("unavailable");
    expect(rowsWritten("{numFiles -> 2, numOutputRows -> 1200, numOutputBytes -> 9}")).toBe(1200);
    expect(rowsWritten('{"numOutputRows":"35"}')).toBe(35);
    expect(rowsWritten(null)).toBeNull();
  });
});
