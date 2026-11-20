// Table explorer: Partitions (each partition with its bound and size, where
// the database has them) and Permissions (who may do what on the table),
// per database, from its catalog. Each is one read the engine checks again
// (origin `explorer`), so a name here can only make a query fail, never
// write. What a database can't say comes back as `{ unavailable }` with why.
import type { ConnectorKind } from "./bindings/ConnectorKind";
import { traitsOf } from "./sourceTraits";
import { qualifiedName, quoteIdent } from "./util";
import type { ExploreRows } from "./bindings/ExploreRows";
import { split, sqlString, type Built } from "./explore";

/** Most partitions listed (Oracle and others can have thousands). */
export const PARTITIONS_MAX = 1000;

const elsewhere = (kind: ConnectorKind, local?: "results" | "files"): string | null => {
  if (traitsOf(kind).preview === "mongo_find") return "MongoDB collections aren't partitioned (a sharded collection's chunks are the cluster's)";
  if (traitsOf(kind).preview === "cypher") return "Neo4j labels aren't tables";
  if (local) return local === "results" ? "A result kept by DataBrain isn't partitioned" : "A file read by DuckDB isn't partitioned here (a Hive-partitioned folder is read as files)";
  return null;
};

/**
 * The table's partitions: rows with `name`, and as the database has them
 * `bound`, `row_count`, `bytes` and more (the first {@link PARTITIONS_MAX}).
 * No rows: the table isn't partitioned. BigQuery bills the read (its
 * `INFORMATION_SCHEMA`), so it is read on demand there.
 */
export function partitionsSql(kind: ConnectorKind, schema: string, table: string, objectKind: string, local?: "results" | "files", redshift = false): Built {
  const why = elsewhere(kind, local);
  if (why) return { unavailable: why };
  if (redshift) return { unavailable: "Redshift tables aren't partitioned (their distribution and sort keys are in the DDL)" };
  if (objectKind === "view") return { unavailable: "A view has no partitions of its own: it runs its query each time it is read" };
  const s = (v: string) => sqlString(kind, v);
  const [db, sch] = split(kind, schema);
  switch (kind) {
    case "postgres":
      return {
        sql: `SELECT c.relname AS name,
  pg_get_expr(c.relpartbound, c.oid) AS bound,
  c.reltuples::bigint AS row_count,
  pg_total_relation_size(c.oid) AS bytes,
  CASE WHEN c.relkind = 'p' THEN 'partitioned' ELSE '' END AS subpartitioned
FROM pg_inherits i
JOIN pg_class c ON c.oid = i.inhrelid
JOIN pg_class p ON p.oid = i.inhparent
JOIN pg_namespace ns ON ns.oid = p.relnamespace
WHERE ns.nspname = ${s(schema)} AND p.relname = ${s(table)}
ORDER BY c.relname
LIMIT ${PARTITIONS_MAX}`,
      };
    case "mysql":
      return {
        sql: `SELECT PARTITION_NAME AS name,
  SUBPARTITION_NAME AS subpartition,
  PARTITION_METHOD AS method,
  PARTITION_DESCRIPTION AS bound,
  TABLE_ROWS AS row_count,
  DATA_LENGTH + INDEX_LENGTH AS bytes
FROM information_schema.PARTITIONS
WHERE TABLE_SCHEMA = ${s(schema)} AND TABLE_NAME = ${s(table)} AND PARTITION_NAME IS NOT NULL
ORDER BY PARTITION_ORDINAL_POSITION, SUBPARTITION_ORDINAL_POSITION
LIMIT ${PARTITIONS_MAX}`,
      };
    case "mssql": {
      // Every table has one partition at least; a partition scheme's
      // boundaries are its RANGE values.
      const p = db ? `${quoteIdent(kind, db)}.sys.` : "sys.";
      return {
        sql: `SELECT TOP ${PARTITIONS_MAX} p.partition_number AS name,
  CONVERT(nvarchar(4000), prv.value, 121) AS bound,
  p.rows AS row_count,
  (SELECT SUM(a.total_pages) * 8192 FROM ${p}allocation_units a WHERE a.container_id = p.partition_id) AS bytes,
  ps.name AS scheme,
  p.data_compression_desc AS compression
FROM ${p}partitions p
JOIN ${p}objects o ON o.object_id = p.object_id
JOIN ${p}schemas sc ON sc.schema_id = o.schema_id
JOIN ${p}indexes ix ON ix.object_id = p.object_id AND ix.index_id = p.index_id
LEFT JOIN ${p}partition_schemes ps ON ps.data_space_id = ix.data_space_id
LEFT JOIN ${p}partition_range_values prv ON prv.function_id = ps.function_id AND prv.boundary_id = p.partition_number
WHERE sc.name = N${s(sch)} AND o.name = N${s(table)} AND p.index_id IN (0, 1)
ORDER BY p.partition_number`,
      };
    }
    case "oracle":
      // Bounded by position: HIGH_VALUE is a LONG (no subquery or ROWNUM around it).
      return {
        sql: `SELECT partition_position AS position,
  partition_name AS name,
  high_value AS bound,
  num_rows AS row_count,
  subpartition_count AS subpartitions,
  tablespace_name AS tablespace,
  compression,
  last_analyzed
FROM all_tab_partitions
WHERE table_owner = ${s(schema)} AND table_name = ${s(table)} AND partition_position <= ${PARTITIONS_MAX}
ORDER BY partition_position`,
      };
    case "bigquery":
      return {
        sql: `SELECT partition_id AS name,
  total_rows AS row_count,
  total_logical_bytes AS bytes,
  storage_tier,
  last_modified_time AS last_changed
FROM \`${[db, sch].filter(Boolean).join(".").replace(/`/g, "")}.INFORMATION_SCHEMA.PARTITIONS\`
WHERE table_name = ${s(table)}
ORDER BY partition_id DESC
LIMIT ${PARTITIONS_MAX}`,
      };
    case "databricks":
      // A table that isn't partitioned answers with an error (said as such).
      return { sql: `SHOW PARTITIONS ${qualifiedName(kind, schema, table)}` };
    case "clickhouse":
      return {
        sql: `SELECT partition AS name,
  sum(rows) AS row_count,
  sum(bytes_on_disk) AS bytes,
  count() AS parts,
  max(modification_time) AS last_changed
FROM system.parts
WHERE database = ${s(schema)} AND table = ${s(table)} AND active
GROUP BY partition
ORDER BY partition
LIMIT ${PARTITIONS_MAX}`,
      };
    case "snowflake":
      return { unavailable: "Snowflake divides every table into micro-partitions itself: its clustering key is in Indexes and Statistics" };
    case "duckdb":
      return { unavailable: "DuckDB tables aren't partitioned (a Hive-partitioned folder is read as files)" };
    case "sqlite":
      return { unavailable: "SQLite tables aren't partitioned" };
    default:
      return { unavailable: `${traitsOf(kind).name} partitions aren't read yet` };
  }
}

/** A Databricks table that isn't partitioned (or can't be) answers SHOW PARTITIONS with an error. */
export const notPartitioned = (message: string) => /not partitioned|NOT_PARTITIONED|is not a partitioned|PARTITION_MANAGEMENT_IS_UNSUPPORTED|does not support partition management/i.test(message);

/**
 * Rows that mean the table has no partitions: BigQuery's one row with no
 * partition id, ClickHouse's one `tuple()` partition, SQL Server's one
 * partition without a bound.
 */
export function unpartitioned(kind: ConnectorKind, rows: ExploreRows): boolean {
  if (rows.rows.length === 0) return true;
  if (rows.rows.length !== 1) return false;
  const at = (c: string) => rows.columns.findIndex((x) => x.toLowerCase() === c);
  const v = (c: string) => rows.rows[0][at(c)] ?? null;
  if (kind === "bigquery") return v("name") == null || v("name") === "__UNPARTITIONED__";
  if (kind === "clickhouse") return v("name") === "tuple()";
  if (kind === "mssql") return v("bound") == null;
  return false;
}

/**
 * Who may do what on the table: forms tried in order until one runs (rows
 * with `grantee`, `privilege` and, as the database has them, `grantable`,
 * `grantor`, `scope` (table, a column, the schema)). Snowflake and
 * Databricks answer `SHOW GRANTS` with their own columns ({@link GRANT_COLUMNS}).
 * Catalogs list what the user may see: some only the user's own grants.
 */
export function permissionsSql(kind: ConnectorKind, schema: string, table: string, objectKind: string, local?: "results" | "files", redshift = false): Built {
  if (traitsOf(kind).preview === "mongo_find") return { unavailable: "MongoDB grants roles on a database, not on a collection" };
  if (traitsOf(kind).preview === "cypher") return { unavailable: "Neo4j grants privileges on the graph, not on a label here" };
  if (local) return { unavailable: "A result or a file read by DuckDB has no permissions of its own" };
  const s = (v: string) => sqlString(kind, v);
  const [db, sch] = split(kind, schema);
  switch (kind) {
    case "postgres": {
      const where = `ns.nspname = ${s(schema)} AND c.relname = ${s(table)}`;
      // Redshift has no aclexplode or LATERAL: what information_schema shows.
      if (redshift) return { sql: `SELECT grantee, privilege_type AS privilege, is_grantable AS grantable, grantor, 'table' AS scope
FROM information_schema.table_privileges
WHERE table_schema = ${s(schema)} AND table_name = ${s(table)}
ORDER BY 1, 2` };
      // The ACL itself (no ACL: the owner's default rights).
      return { sql: `SELECT CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_get_userbyid(a.grantee) END AS grantee,
  a.privilege_type AS privilege,
  CASE WHEN a.is_grantable THEN 'yes' ELSE 'no' END AS grantable,
  pg_get_userbyid(a.grantor) AS grantor,
  'table' AS scope
FROM pg_class c
JOIN pg_namespace ns ON ns.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
WHERE ${where}
UNION ALL
SELECT CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_get_userbyid(a.grantee) END,
  a.privilege_type,
  CASE WHEN a.is_grantable THEN 'yes' ELSE 'no' END,
  pg_get_userbyid(a.grantor),
  'column ' || att.attname
FROM pg_class c
JOIN pg_namespace ns ON ns.oid = c.relnamespace
JOIN pg_attribute att ON att.attrelid = c.oid AND att.attnum > 0 AND NOT att.attisdropped AND att.attacl IS NOT NULL
CROSS JOIN LATERAL aclexplode(att.attacl) a
WHERE ${where}
ORDER BY 1, 5, 2` };
    }
    case "mysql":
      return {
          sql: `SELECT GRANTEE AS grantee, PRIVILEGE_TYPE AS privilege, IS_GRANTABLE AS grantable, 'table' AS scope
FROM information_schema.TABLE_PRIVILEGES
WHERE TABLE_SCHEMA = ${s(schema)} AND TABLE_NAME = ${s(table)}
UNION ALL
SELECT GRANTEE, PRIVILEGE_TYPE, IS_GRANTABLE, CONCAT('column ', COLUMN_NAME)
FROM information_schema.COLUMN_PRIVILEGES
WHERE TABLE_SCHEMA = ${s(schema)} AND TABLE_NAME = ${s(table)}
UNION ALL
SELECT GRANTEE, PRIVILEGE_TYPE, IS_GRANTABLE, 'schema'
FROM information_schema.SCHEMA_PRIVILEGES
WHERE TABLE_SCHEMA = ${s(schema)}
UNION ALL
SELECT GRANTEE, PRIVILEGE_TYPE, IS_GRANTABLE, 'all databases'
FROM information_schema.USER_PRIVILEGES
WHERE PRIVILEGE_TYPE IN ('SELECT', 'INSERT', 'UPDATE', 'DELETE', 'ALTER', 'DROP', 'INDEX', 'TRIGGER', 'REFERENCES', 'CREATE VIEW', 'SHOW VIEW')
ORDER BY 1, 4, 2`,
      };
    case "mssql": {
      const p = db ? `${quoteIdent(kind, db)}.sys.` : "sys.";
      return {
          sql: `SELECT pr.name AS grantee, pe.permission_name AS privilege, pe.state_desc AS state,
  CASE WHEN pe.minor_id = 0 THEN 'table' ELSE 'column ' + ISNULL(col.name, CAST(pe.minor_id AS nvarchar(10))) END AS scope,
  gr.name AS grantor
FROM ${p}database_permissions pe
JOIN ${p}database_principals pr ON pr.principal_id = pe.grantee_principal_id
JOIN ${p}database_principals gr ON gr.principal_id = pe.grantor_principal_id
JOIN ${p}objects o ON o.object_id = pe.major_id
JOIN ${p}schemas sc ON sc.schema_id = o.schema_id
LEFT JOIN ${p}columns col ON col.object_id = pe.major_id AND col.column_id = pe.minor_id
WHERE pe.class = 1 AND sc.name = N${s(sch)} AND o.name = N${s(table)}
UNION ALL
SELECT pr.name, pe.permission_name, pe.state_desc, 'schema', gr.name
FROM ${p}database_permissions pe
JOIN ${p}database_principals pr ON pr.principal_id = pe.grantee_principal_id
JOIN ${p}database_principals gr ON gr.principal_id = pe.grantor_principal_id
JOIN ${p}schemas sc ON sc.schema_id = pe.major_id
WHERE pe.class = 3 AND sc.name = N${s(sch)}
ORDER BY 1, 4, 2`,
      };
    }
    case "oracle":
      return {
          sql: `SELECT grantee, privilege, grantable, grantor, 'table' AS scope
FROM all_tab_privs
WHERE table_schema = ${s(schema)} AND table_name = ${s(table)}
UNION ALL
SELECT grantee, privilege, grantable, grantor, 'column ' || column_name
FROM all_col_privs
WHERE table_schema = ${s(schema)} AND table_name = ${s(table)}
ORDER BY 1, 5, 2`,
      };
    case "snowflake":
      return { sql: `SHOW GRANTS ON ${objectKind === "view" ? "VIEW" : objectKind === "materialized_view" ? "MATERIALIZED VIEW" : objectKind === "foreign_table" ? "EXTERNAL TABLE" : "TABLE"} ${qualifiedName(kind, schema, table)}` };
    case "databricks":
      return { sql: `SHOW GRANTS ON ${objectKind === "view" ? "VIEW" : "TABLE"} ${qualifiedName(kind, schema, table)}` };
    case "clickhouse":
      return {
          sql: `SELECT coalesce(user_name, role_name) AS grantee,
  access_type AS privilege,
  if(grant_option, 'yes', 'no') AS grantable,
  multiIf(database IS NULL, 'all databases', table IS NULL, 'database', column IS NULL, 'table', concat('column ', column)) AS scope,
  if(is_partial_revoke, 'revoked', '') AS revoked
FROM system.grants
WHERE (database IS NULL OR database = ${s(schema)}) AND (table IS NULL OR table = ${s(table)})
ORDER BY grantee, scope, privilege`,
      };
    case "bigquery":
      return { unavailable: "BigQuery grants access through IAM on the project, dataset or table (the console's Sharing), which isn't read here" };
    case "duckdb":
    case "sqlite":
      return { unavailable: `${traitsOf(kind).name} has no users or permissions: whoever can open the database file can read and change it` };
    default:
      return { unavailable: `${traitsOf(kind).name} permissions aren't read yet` };
  }
}

/** The `SHOW GRANTS` columns shown (Snowflake, Databricks), in order; other answers show every column. */
export const GRANT_COLUMNS: Partial<Record<ConnectorKind, string[]>> = {
  snowflake: ["grantee_name", "granted_to", "privilege", "grant_option", "granted_by"],
  databricks: ["principal", "actiontype", "objecttype", "objectkey"],
};

/** Headers of the columns these reads name. */
export const COLUMN_LABELS: Record<string, string> = {
  grantee: "Grantee",
  grantee_name: "Grantee",
  granted_to: "Kind",
  principal: "Principal",
  privilege: "Privilege",
  actiontype: "Privilege",
  objecttype: "On",
  objectkey: "Object",
  grantable: "Can grant",
  grant_option: "Can grant",
  grantor: "Granted by",
  granted_by: "Granted by",
  scope: "On",
  state: "State",
  revoked: "Revoked",
  position: "#",
  name: "Partition",
  subpartition: "Subpartition",
  subpartitioned: "Sub-partitioned",
  method: "Method",
  bound: "Bound",
  row_count: "Rows (estimate)",
  bytes: "Size",
  parts: "Parts",
  subpartitions: "Subpartitions",
  tablespace: "Tablespace",
  compression: "Compression",
  scheme: "Scheme",
  storage_tier: "Storage",
  last_analyzed: "Last analyzed",
  last_changed: "Last changed",
};
