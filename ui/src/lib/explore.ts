// Table explorer, step 2: the SQL of a column profile, a column's top
// values, an exact count and the table's statistics, per database. Each is
// one read the engine checks again (origin `explorer`), so a name here can
// only make a query fail, never write. What a database can't say comes back
// as `{ unavailable }` with why, shown in the tab (never hidden).
import type { ConnectorKind } from "./bindings/ConnectorKind";
import { traitsOf } from "./sourceTraits";
import type { CostEstimate } from "./bindings/CostEstimate";
import type { ExploreRows } from "./bindings/ExploreRows";
import { formatBytes, qualifiedName, quoteIdent, shortCount } from "./util";

export type Built = { sql: string } | { unavailable: string };

/** What a profile computes for a column. */
export type ColumnClass = "number" | "time" | "text" | "other";

/** Most columns one profile query covers (one row, four values a column). */
export const PROFILE_MAX_COLUMNS = 100;

/** Top values listed for a column. */
export const TOP_VALUES = 10;

/**
 * A database that bills a query by the bytes it scans, whatever its LIMIT
 * (BigQuery): its sample doesn't run by itself, and Run says what it reads.
 */
export const billsScans = (kind: string) => kind === "bigquery";

/** Past this many rows (the database's estimate) a read of every row asks first. */
export const LARGE_TABLE_ROWS = 1_000_000;

/**
 * Why a read of every row (profile, top values, exact count) asks first;
 * empty: it runs. The scan's bill (`estimate`, BigQuery) is asked about at
 * each read, as each one bills what it scans: a count can cost nothing and
 * a profile scan every column. The rest (production, a large table, or a
 * server database that doesn't say how many rows it has) only until the
 * tab has `asked` once.
 */
export function readAllReasons(o: {
  kind: ConnectorKind;
  prod: boolean;
  connName: string;
  table: string;
  rowEstimate: number | null;
  asked: boolean;
  estimate?: CostEstimate | null;
}): string[] {
  const reasons: string[] = [];
  if (billsScans(o.kind)) {
    const est = o.estimate;
    if (est?.bytes == null) reasons.push("BigQuery bills the columns this query scans; the estimate couldn't be read.");
    else if (est.bytes > 0 || est.over_limit != null) reasons.push(`This query reads about ${formatBytes(est.bytes)}: BigQuery bills the columns it scans.`);
    if (est?.over_limit != null) reasons.push(`That is over the connection's limit of ${formatBytes(est.over_limit)} billed: BigQuery will refuse it.`);
  }
  if (o.asked) return reasons;
  if (o.prod) reasons.push(`"${o.connName}" is a production connection.`);
  if (o.rowEstimate != null && o.rowEstimate > LARGE_TABLE_ROWS) reasons.push(`${o.table} has about ${shortCount(o.rowEstimate)} rows: this reads all of them.`);
  else if (o.rowEstimate == null && !traitsOf(o.kind).local) reasons.push(`The database doesn't say how many rows ${o.table} has: this reads all of them, however many.`);
  return reasons;
}

/** Session keys of a table tab's explorer reads, to close with the tab. */
const sessions = new Map<string, Set<string>>();

/**
 * The session key of an explorer read: `{tabId}#{what}`, its own so one
 * read doesn't wait for another, and noted to be closed (and stopped) with
 * the tab ({@link takeExploreKeys}).
 */
export function exploreKey(tabId: string, what: string): string {
  const key = `${tabId}#${what}`;
  let keys = sessions.get(tabId);
  if (!keys) sessions.set(tabId, (keys = new Set()));
  keys.add(key);
  return key;
}

/** The session keys a tab's explorer used, forgotten: the caller closes them. */
export function takeExploreKeys(tabId: string): string[] {
  const keys = sessions.get(tabId);
  sessions.delete(tabId);
  return keys ? [...keys] : [];
}

/** Databases without SQL tables: no profile, statistics or count yet. */
const noSql = (kind: ConnectorKind): string | null =>
  traitsOf(kind).preview === "mongo_find"
    ? "MongoDB documents have no fixed columns: the explorer doesn't profile collections yet"
    : traitsOf(kind).preview === "cypher"
      ? "Neo4j labels aren't tables: the explorer doesn't profile them yet"
      : null;

/**
 * What can be counted and compared in a column, from its type as the
 * database names it. Types that can't be compared or counted distinct
 * everywhere (JSON, arrays, LOBs, XML, geometry, binary) are `other`: only
 * their non-null share is read.
 */
export function columnClass(kind: ConnectorKind, dataType: string): ColumnClass {
  // ClickHouse wraps types: Nullable(Int32), LowCardinality(String).
  const t = dataType
    .toLowerCase()
    .replace(/^lowcardinality\((.*)\)$/, "$1")
    .replace(/^nullable\((.*)\)$/, "$1")
    .trim();
  // Large objects and documents first: `text` is a LOB in SQL Server, `long` in Oracle.
  if (/\b(clob|nclob|blob|bfile|long|image|ntext|xml|json|jsonb|variant|object|array|struct|map|geography|geometry|bytea|binary|varbinary|raw|hierarchyid|sql_variant|tsvector|super)\b/.test(t) || t.endsWith("[]")) return "other";
  if (kind === "mssql" && /^text\b/.test(t)) return "other";
  if (/\binterval\b/.test(t)) return "other";
  if (/^(u?int\d*|tinyint|smallint|mediumint|bigint|int|integer|serial|bigserial|smallserial|number|numeric|bignumeric|decimal\d*|dec|real|float\d*|double|double precision|binary_float|binary_double|money|smallmoney|hugeint|uhugeint|utinyint|usmallint|uinteger|ubigint)\b/.test(t)) return "number";
  if (/^(date\d*|datetime\d*|smalldatetime|datetimeoffset|time|timestamp|timestamptz|timestamp_ntz|timestamp_ltz|timestamp_tz|year)\b/.test(t)) return "time";
  if (/^(bool|boolean|bit|uuid|uniqueidentifier|enum\d*|inet|cidr|macaddr)\b/.test(t)) return "text";
  if (/^(char|nchar|varchar|nvarchar|varchar2|nvarchar2|character|character varying|text|string|tinytext|mediumtext|longtext|citext|name|fixedstring)\b/.test(t)) return "text";
  return "other";
}

/** The distinct count: approximate where the database has it (a full exact count is slow on a large table). */
function distinct(kind: ConnectorKind, col: string): { sql: string; approx: boolean } {
  switch (kind) {
    case "snowflake":
    case "bigquery":
    case "oracle":
      return { sql: `APPROX_COUNT_DISTINCT(${col})`, approx: true };
    case "databricks":
    case "duckdb":
      return { sql: `approx_count_distinct(${col})`, approx: true };
    case "clickhouse":
      return { sql: `uniq(${col})`, approx: true };
    default:
      // SQL Server's APPROX_COUNT_DISTINCT needs 2019; PostgreSQL, MySQL and SQLite have none.
      return { sql: `COUNT(DISTINCT ${col})`, approx: false };
  }
}

/** Whether the profile's distinct counts are approximate on this database. */
export const approxDistinct = (kind: ConnectorKind) => distinct(kind, "x").approx;

const fromOf = (kind: ConnectorKind, schema: string, table: string, from?: string) => from ?? qualifiedName(kind, schema, table);

/**
 * One query, one scan: for each column (the first {@link PROFILE_MAX_COLUMNS})
 * its non-null count `n{i}`, distinct count `d{i}` (not for `other`), and
 * min `lo{i}` / max `hi{i}` (numbers and times), plus `total`, the rows.
 */
export function profileSql(kind: ConnectorKind, schema: string, table: string, columns: { name: string; data_type: string }[], from?: string): Built {
  const why = noSql(kind);
  if (why) return { unavailable: why };
  if (!columns.length) return { unavailable: "The database lists no columns for it" };
  const parts = ["COUNT(*) AS total"];
  columns.slice(0, PROFILE_MAX_COLUMNS).forEach((c, i) => {
    const q = quoteIdent(kind, c.name);
    const cls = columnClass(kind, c.data_type);
    parts.push(`COUNT(${q}) AS n${i}`);
    if (cls !== "other") parts.push(`${distinct(kind, q).sql} AS d${i}`);
    if (cls === "number" || cls === "time") parts.push(`MIN(${q}) AS lo${i}`, `MAX(${q}) AS hi${i}`);
  });
  return { sql: `SELECT\n  ${parts.join(",\n  ")}\nFROM ${fromOf(kind, schema, table, from)}` };
}

/** A column's {@link TOP_VALUES} most common values with their counts (`value`, `n`). */
export function topValuesSql(kind: ConnectorKind, schema: string, table: string, column: string, from?: string): Built {
  const why = noSql(kind);
  if (why) return { unavailable: why };
  const q = quoteIdent(kind, column);
  const body = `${q} AS value, COUNT(*) AS n\nFROM ${fromOf(kind, schema, table, from)}\nGROUP BY ${q}\nORDER BY n DESC`;
  switch (traitsOf(kind).row_limit) {
    case "top":
      return { sql: `SELECT TOP ${TOP_VALUES} ${body}` };
    case "fetch_first":
      return { sql: `SELECT ${body}\nFETCH FIRST ${TOP_VALUES} ROWS ONLY` };
    default:
      return { sql: `SELECT ${body}\nLIMIT ${TOP_VALUES}` };
  }
}

/** Count every row (`n`). */
export function countSql(kind: ConnectorKind, schema: string, table: string, from?: string): Built {
  const why = noSql(kind);
  if (why) return { unavailable: why };
  return { sql: `SELECT COUNT(*) AS n\nFROM ${fromOf(kind, schema, table, from)}` };
}

/** A string literal: doubled quotes, or backslash escapes where the database reads them in strings. */
export function sqlString(kind: ConnectorKind, s: string): string {
  if (kind === "mysql" || kind === "clickhouse" || kind === "bigquery" || kind === "databricks") return "'" + s.replace(/\\/g, "\\\\").replace(/'/g, "\\'") + "'";
  return "'" + s.replace(/'/g, "''") + "'";
}

/** `db.schema` of three-level names (the first dot), else `[null, schema]`. */
export function split(kind: ConnectorKind, schema: string): [string | null, string] {
  const levels = traitsOf(kind).levels;
  const i = schema.indexOf(".");
  if ((levels === "three" || levels === "three_without_database") && i > 0) return [schema.slice(0, i), schema.slice(i + 1)];
  return [null, schema];
}

/** Kinds of objects that keep statistics of their own. */
export const hasStatistics = (objectKind: string) => objectKind === "table" || objectKind === "materialized_view" || objectKind === "foreign_table";

/**
 * The table's statistics as the database's catalog keeps them: one row
 * whose columns are the figures (`row_estimate`, `total_bytes`, …; names
 * the tab knows get a label, others are shown as named). Nothing is
 * counted or scanned.
 */
export function statisticsSql(kind: ConnectorKind, schema: string, table: string, objectKind: string, local?: "results" | "files"): Built {
  const why = noSql(kind);
  if (why) return { unavailable: why.replace("profile", "read statistics of") };
  if (local) return { unavailable: local === "results" ? "A result kept by DataBrain has no catalog statistics: Count exactly counts its rows" : "A file read by DuckDB has no catalog statistics: Count exactly counts its rows" };
  if (!hasStatistics(objectKind)) return { unavailable: "A view keeps no statistics of its own: it runs its query each time it is read" };
  const s = (v: string) => sqlString(kind, v);
  const [db, sch] = split(kind, schema);
  switch (kind) {
    case "postgres":
      return {
        sql: `SELECT c.reltuples::bigint AS row_estimate,
  pg_total_relation_size(c.oid) AS total_bytes,
  pg_relation_size(c.oid) AS table_bytes,
  pg_indexes_size(c.oid) AS index_bytes,
  (SELECT count(*) FROM pg_inherits i WHERE i.inhparent = c.oid) AS partitions,
  st.n_live_tup AS live_rows,
  st.n_dead_tup AS dead_rows,
  st.seq_scan AS seq_scans,
  st.idx_scan AS index_scans,
  st.n_tup_ins AS rows_inserted,
  st.n_tup_upd AS rows_updated,
  st.n_tup_del AS rows_deleted,
  greatest(st.last_analyze, st.last_autoanalyze) AS last_analyzed,
  greatest(st.last_vacuum, st.last_autovacuum) AS last_vacuumed
FROM pg_class c
JOIN pg_namespace ns ON ns.oid = c.relnamespace
LEFT JOIN pg_stat_all_tables st ON st.relid = c.oid
WHERE ns.nspname = ${s(schema)} AND c.relname = ${s(table)}`,
      };
    case "mysql":
      return {
        sql: `SELECT t.TABLE_ROWS AS row_estimate,
  t.DATA_LENGTH + t.INDEX_LENGTH AS total_bytes,
  t.DATA_LENGTH AS table_bytes,
  t.INDEX_LENGTH AS index_bytes,
  t.DATA_FREE AS free_bytes,
  (SELECT COUNT(*) FROM information_schema.PARTITIONS p WHERE p.TABLE_SCHEMA = t.TABLE_SCHEMA AND p.TABLE_NAME = t.TABLE_NAME AND p.PARTITION_NAME IS NOT NULL) AS partitions,
  t.ENGINE AS engine,
  t.CREATE_TIME AS created,
  t.UPDATE_TIME AS last_changed
FROM information_schema.TABLES t
WHERE t.TABLE_SCHEMA = ${s(schema)} AND t.TABLE_NAME = ${s(table)}`,
      };
    case "mssql": {
      const p = db ? `${quoteIdent(kind, db)}.sys.` : "sys.";
      return {
        sql: `SELECT
  (SELECT SUM(p.rows) FROM ${p}partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) AS row_estimate,
  (SELECT SUM(a.total_pages) * 8192 FROM ${p}partitions p JOIN ${p}allocation_units a ON a.container_id = p.partition_id WHERE p.object_id = o.object_id) AS total_bytes,
  (SELECT SUM(a.total_pages) * 8192 FROM ${p}partitions p JOIN ${p}allocation_units a ON a.container_id = p.partition_id WHERE p.object_id = o.object_id AND p.index_id > 1) AS index_bytes,
  (SELECT COUNT(DISTINCT p.partition_number) FROM ${p}partitions p WHERE p.object_id = o.object_id) AS partitions,
  o.create_date AS created,
  o.modify_date AS last_ddl
FROM ${p}objects o
JOIN ${p}schemas sc ON sc.schema_id = o.schema_id
WHERE sc.name = N${s(sch)} AND o.name = N${s(table)}`,
      };
    }
    case "oracle":
      return {
        sql: `SELECT t.num_rows AS row_estimate,
  t.blocks AS blocks,
  t.avg_row_len AS avg_row_bytes,
  (SELECT COUNT(*) FROM all_tab_partitions p WHERE p.table_owner = t.owner AND p.table_name = t.table_name) AS partitions,
  t.last_analyzed AS last_analyzed,
  o.created AS created,
  o.last_ddl_time AS last_ddl
FROM all_tables t
JOIN all_objects o ON o.owner = t.owner AND o.object_name = t.table_name AND o.object_type = 'TABLE'
WHERE t.owner = ${s(schema)} AND t.table_name = ${s(table)}`,
      };
    case "snowflake":
      return {
        sql: `SELECT row_count AS row_estimate,
  bytes AS total_bytes,
  clustering_key,
  retention_time AS time_travel_days,
  created,
  last_altered AS last_changed
FROM ${db ? `${quoteIdent(kind, db)}.` : ""}information_schema.tables
WHERE table_schema = ${s(sch)} AND table_name = ${s(table)}`,
      };
    case "bigquery":
      // __TABLES__ is metadata: reading it scans no table data.
      return {
        sql: `SELECT row_count AS row_estimate,
  size_bytes AS total_bytes,
  TIMESTAMP_MILLIS(creation_time) AS created,
  TIMESTAMP_MILLIS(last_modified_time) AS last_changed
FROM \`${[db, sch].filter(Boolean).join(".").replace(/`/g, "")}.__TABLES__\`
WHERE table_id = ${s(table)}`,
      };
    case "databricks":
      // Delta tables (the others say why in the error).
      return { sql: `DESCRIBE DETAIL ${qualifiedName(kind, schema, table)}` };
    case "duckdb":
      return {
        sql: `SELECT estimated_size AS row_estimate,
  column_count,
  index_count,
  has_primary_key
FROM duckdb_tables()
WHERE ${db ? `database_name = ${s(db)} AND ` : ""}schema_name = ${s(sch)} AND table_name = ${s(table)}`,
      };
    case "clickhouse":
      return {
        sql: `SELECT total_rows AS row_estimate,
  total_bytes,
  engine,
  partition_key,
  sorting_key,
  metadata_modification_time AS last_ddl
FROM system.tables
WHERE database = ${s(schema)} AND name = ${s(table)}`,
      };
    case "sqlite":
      return { unavailable: "SQLite keeps no statistics about a table: Count exactly counts its rows" };
    default:
      return { unavailable: `${traitsOf(kind).name} statistics aren't read yet` };
  }
}

/** Labels and formats of the figures {@link statisticsSql} reads (others are shown as named). */
export const STAT_LABELS: Record<string, { label: string; format?: "bytes" | "count" }> = {
  row_estimate: { label: "Rows (the catalog's figure)", format: "count" },
  total_bytes: { label: "Size, with indexes", format: "bytes" },
  sizeinbytes: { label: "Size", format: "bytes" },
  table_bytes: { label: "Table size", format: "bytes" },
  index_bytes: { label: "Index size", format: "bytes" },
  free_bytes: { label: "Free (allocated, unused)", format: "bytes" },
  uncompressed_bytes: { label: "Size uncompressed", format: "bytes" },
  partitions: { label: "Partitions", format: "count" },
  numfiles: { label: "Files", format: "count" },
  live_rows: { label: "Live rows", format: "count" },
  dead_rows: { label: "Dead rows (to vacuum)", format: "count" },
  seq_scans: { label: "Full scans", format: "count" },
  index_scans: { label: "Index scans", format: "count" },
  rows_inserted: { label: "Rows inserted (since stats reset)", format: "count" },
  rows_updated: { label: "Rows updated (since stats reset)", format: "count" },
  rows_deleted: { label: "Rows deleted (since stats reset)", format: "count" },
  blocks: { label: "Blocks", format: "count" },
  avg_row_bytes: { label: "Average row", format: "bytes" },
  last_analyzed: { label: "Last analyzed" },
  last_vacuumed: { label: "Last vacuumed" },
  last_changed: { label: "Last changed" },
  lastmodified: { label: "Last changed" },
  last_ddl: { label: "Definition last changed" },
  created: { label: "Created" },
  createdat: { label: "Created" },
  engine: { label: "Engine" },
  format: { label: "Format" },
  location: { label: "Location" },
  partitioncolumns: { label: "Partitioned by" },
  partition_key: { label: "Partitioned by" },
  sorting_key: { label: "Sorted by" },
  clustering_key: { label: "Clustered by" },
  clusteringcolumns: { label: "Clustered by" },
  time_travel_days: { label: "Time travel (days)", format: "count" },
  column_count: { label: "Columns", format: "count" },
  index_count: { label: "Indexes", format: "count" },
  has_primary_key: { label: "Has a primary key" },
};

/** A cell's text as a number, when it is one. */
export const num = (v: string | null | undefined) => (v != null && v.trim() !== "" && Number.isFinite(Number(v)) ? Number(v) : null);

/** One column's figures from a profile row. */
export type ColumnProfile = { nonNull: number | null; distinct: number | null; lo: string | null; hi: string | null };

/** The profile row read back: `total` and each column's `n{i}`, `d{i}`, `lo{i}`, `hi{i}` (any case: Oracle and Snowflake answer in upper case). */
export function readProfile(rows: ExploreRows, count: number): { total: number | null; columns: ColumnProfile[] } {
  const at = new Map(rows.columns.map((c, i) => [c.toLowerCase(), i]));
  const row = rows.rows[0] ?? [];
  const get = (k: string) => {
    const i = at.get(k);
    return i == null ? null : (row[i] ?? null);
  };
  return {
    total: num(get("total")),
    columns: Array.from({ length: count }, (_, i) => {
      const nonNull = num(get(`n${i}`));
      const d = num(get(`d${i}`));
      // An approximate count can come out above the values there are (DuckDB: 1,249 of 1,000).
      return { nonNull, distinct: d != null && nonNull != null ? Math.min(d, nonNull) : d, lo: get(`lo${i}`), hi: get(`hi${i}`) };
    }),
  };
}


// ------------------------------------------------------------------ step 4: constraints, index use, history

/** Why a database has no checks or triggers here, or `null` when it has them. */
function noConstraints(kind: ConnectorKind, local?: "results" | "files"): string | null {
  if (kind === "mongodb") return "MongoDB has no constraints or triggers on a collection (its validator is in the collection's options)";
  if (kind === "neo4j") return "Neo4j constraints are on labels, not read here yet";
  if (local) return "A result or a file read by DuckDB has no constraints";
  if (kind === "snowflake" || kind === "bigquery") return `${traitsOf(kind).name} has no check constraints or triggers (its keys are informational, listed in Columns)`;
  if (kind === "clickhouse") return "ClickHouse checks are in the table's DDL (the DDL part), and it has no triggers";
  return null;
}

/**
 * The table's check constraints (unique keys and the primary key are among
 * its indexes), as alternatives tried in order until one runs (MariaDB's
 * form, then MySQL's; Oracle 12.2+, then older): rows of `what` (`check`,
 * `exclusion`), `name`, `definition`. Databricks answers with its table
 * properties (`key`, `value`): read with {@link readConstraints}. Read
 * apart from the triggers, so one failing doesn't lose the other.
 */
export function checksSql(kind: ConnectorKind, schema: string, table: string, local?: "results" | "files"): Built[] {
  const why = noConstraints(kind, local);
  if (why) return [{ unavailable: why }];
  const s = (v: string) => sqlString(kind, v);
  const [db, sch] = split(kind, schema);
  switch (kind) {
    case "postgres":
      return [
        {
          sql: `SELECT CASE con.contype WHEN 'c' THEN 'check' ELSE 'exclusion' END AS what, con.conname AS name, pg_get_constraintdef(con.oid) AS definition
FROM pg_constraint con
JOIN pg_class c ON c.oid = con.conrelid
JOIN pg_namespace ns ON ns.oid = c.relnamespace
WHERE ns.nspname = ${s(schema)} AND c.relname = ${s(table)} AND con.contype IN ('c', 'x')
ORDER BY 2`,
        },
      ];
    case "mysql": {
      // MariaDB names checks per table (CONSTRAINT_1 in several tables):
      // its CHECK_CONSTRAINTS has TABLE_NAME. MySQL's (8.0.16+) names them
      // per schema and has no TABLE_NAME: the second form.
      const body = (extra: string) => `SELECT 'check' AS what, cc.CONSTRAINT_NAME AS name, cc.CHECK_CLAUSE AS definition
FROM information_schema.CHECK_CONSTRAINTS cc
JOIN information_schema.TABLE_CONSTRAINTS tc ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME${extra}
WHERE tc.TABLE_SCHEMA = ${s(schema)} AND tc.TABLE_NAME = ${s(table)} AND tc.CONSTRAINT_TYPE = 'CHECK'`;
      return [{ sql: body(" AND cc.TABLE_NAME = tc.TABLE_NAME") }, { sql: body("") }];
    }
    case "mssql": {
      const p = db ? `${quoteIdent(kind, db)}.sys.` : "sys.";
      return [
        {
          sql: `SELECT 'check' AS what, cc.name AS name, cc.definition AS definition
FROM ${p}check_constraints cc
JOIN ${p}objects o ON o.object_id = cc.parent_object_id
JOIN ${p}schemas sc ON sc.schema_id = o.schema_id
WHERE sc.name = N${s(sch)} AND o.name = N${s(table)}`,
        },
      ];
    }
    case "oracle": {
      // A NOT NULL column is a check the system names: left out (Columns shows it).
      const notNull = "NOT (generated = 'GENERATED NAME' AND search_condition_vc LIKE '% IS NOT NULL')";
      return [
        { sql: `SELECT 'check' AS what, constraint_name AS name, search_condition_vc AS definition
FROM all_constraints
WHERE owner = ${s(schema)} AND table_name = ${s(table)} AND constraint_type = 'C' AND ${notNull}` },
        // Before 12.2 the condition is only a LONG (not readable here): the user-named checks' names.
        { sql: `SELECT 'check' AS what, constraint_name AS name, '(its condition is in the DDL)' AS definition
FROM all_constraints
WHERE owner = ${s(schema)} AND table_name = ${s(table)} AND constraint_type = 'C' AND generated = 'USER NAME'` },
      ];
    }
    case "duckdb":
      return [
        {
          sql: `SELECT 'check' AS what, '' AS name, constraint_text AS definition
FROM duckdb_constraints()
WHERE ${db ? `database_name = ${s(db)} AND ` : ""}schema_name = ${s(sch)} AND table_name = ${s(table)} AND constraint_type = 'CHECK'`,
        },
      ];
    case "sqlite":
      return [{ unavailable: "SQLite checks are only in the table's DDL (the DDL part)" }];
    case "databricks":
      // Delta CHECK constraints are table properties `delta.constraints.<name>`.
      return [{ sql: `SHOW TBLPROPERTIES ${qualifiedName(kind, schema, table)}` }];
    default:
      return [{ unavailable: `${traitsOf(kind).name} constraints aren't read yet` }];
  }
}

/** The table's triggers: rows of `what` (`trigger`), `name`, `definition`. */
export function triggersSql(kind: ConnectorKind, schema: string, table: string, local?: "results" | "files"): Built {
  const why = noConstraints(kind, local);
  if (why) return { unavailable: why };
  const s = (v: string) => sqlString(kind, v);
  const [db, sch] = split(kind, schema);
  switch (kind) {
    case "postgres":
      return {
        sql: `SELECT 'trigger' AS what, t.tgname AS name, pg_get_triggerdef(t.oid) AS definition
FROM pg_trigger t
JOIN pg_class c ON c.oid = t.tgrelid
JOIN pg_namespace ns ON ns.oid = c.relnamespace
WHERE ns.nspname = ${s(schema)} AND c.relname = ${s(table)} AND NOT t.tgisinternal
ORDER BY 2`,
      };
    case "mysql":
      return {
        sql: `SELECT 'trigger' AS what, TRIGGER_NAME AS name, CONCAT(ACTION_TIMING, ' ', EVENT_MANIPULATION, ' FOR EACH ROW ', ACTION_STATEMENT) AS definition
FROM information_schema.TRIGGERS
WHERE EVENT_OBJECT_SCHEMA = ${s(schema)} AND EVENT_OBJECT_TABLE = ${s(table)}`,
      };
    case "mssql": {
      const p = db ? `${quoteIdent(kind, db)}.sys.` : "sys.";
      return {
        sql: `SELECT 'trigger' AS what, tr.name AS name, CASE WHEN tr.is_disabled = 1 THEN 'disabled' ELSE 'enabled' END + CASE WHEN tr.is_instead_of_trigger = 1 THEN ', instead of' ELSE '' END AS definition
FROM ${p}triggers tr
JOIN ${p}objects o ON o.object_id = tr.parent_id
JOIN ${p}schemas sc ON sc.schema_id = o.schema_id
WHERE sc.name = N${s(sch)} AND o.name = N${s(table)}`,
      };
    }
    case "oracle":
      return { sql: `SELECT 'trigger' AS what, trigger_name AS name, trigger_type || ' ' || triggering_event || ' (' || status || ')' AS definition
FROM all_triggers
WHERE table_owner = ${s(schema)} AND table_name = ${s(table)}` };
    case "sqlite":
      return { sql: `SELECT 'trigger' AS what, name, sql AS definition
FROM ${quoteIdent(kind, schema)}.sqlite_master
WHERE type = 'trigger' AND tbl_name = ${s(table)}` };
    case "duckdb":
    case "databricks":
      return { unavailable: `${traitsOf(kind).name} has no triggers` };
    default:
      return { unavailable: `${traitsOf(kind).name} triggers aren't read yet` };
  }
}

/** A check, exclusion or trigger of the table. */
export type Constraint = { what: string; name: string; definition: string };

/** The rows of {@link checksSql} and {@link triggersSql} (Databricks: its `delta.constraints.*` properties). */
export function readConstraints(kind: ConnectorKind, rows: ExploreRows): Constraint[] {
  const at = (n: string) => rows.columns.findIndex((c) => c.toLowerCase() === n);
  if (kind === "databricks") {
    const [k, v] = [at("key"), at("value")];
    return rows.rows.flatMap((r) => {
      const key = r[k] ?? "";
      return key.startsWith("delta.constraints.") ? [{ what: "check", name: key.slice("delta.constraints.".length), definition: r[v] ?? "" }] : [];
    });
  }
  const [w, n, d] = [at("what"), at("name"), at("definition")];
  return rows.rows.map((r) => ({ what: r[w] ?? "", name: r[n] ?? "", definition: r[d] ?? "" }));
}

/** How often each index was used and its size, where the database keeps it (`name`, `scans`, `bytes`). */
export function indexUseSql(kind: ConnectorKind, schema: string, table: string): Built {
  if (kind !== "postgres") return { unavailable: `${traitsOf(kind).name}: index use isn't read` };
  const s = (v: string) => sqlString(kind, v);
  return {
    sql: `SELECT s.indexrelname AS name, s.idx_scan AS scans, pg_relation_size(s.indexrelid) AS bytes
FROM pg_stat_all_indexes s
WHERE s.schemaname = ${s(schema)} AND s.relname = ${s(table)}`,
  };
}

/** Databases that keep earlier versions of a table's rows, and how they are read. */
export type HistoryKind = "versions" | "as_of" | null;

/** Databricks lists versions (Delta history); Snowflake and BigQuery read the rows as of a time (time travel). */
export function historyKind(kind: ConnectorKind): HistoryKind {
  return kind === "databricks" ? "versions" : kind === "snowflake" || kind === "bigquery" ? "as_of" : null;
}

/** The latest versions of a Delta table (Databricks `DESCRIBE HISTORY`). */
export function historySql(kind: ConnectorKind, schema: string, table: string, n = 50): Built {
  if (kind !== "databricks") return { unavailable: "Only Delta tables (Databricks) list their versions" };
  return { sql: `DESCRIBE HISTORY ${qualifiedName(kind, schema, table)} LIMIT ${n}` };
}

/** A time typed for "as of": a date, or a date and time (to the second). */
export const AS_OF_TIME = /^\d{4}-\d{2}-\d{2}(?:[ T]\d{2}:\d{2}(?::\d{2})?)?$/;

/**
 * The first rows of the table as they were: at a Delta `version`, or at a
 * time (`at`, checked against {@link AS_OF_TIME}; the database's time zone).
 * For a query tab: nothing runs until the user runs it.
 */
export function asOfSql(kind: ConnectorKind, schema: string, table: string, at: { version: number } | { time: string }, n = 100): Built {
  const q = qualifiedName(kind, schema, table);
  if ("time" in at && !AS_OF_TIME.test(at.time.trim())) return { unavailable: "Write a date, or a date and time (2026-11-20 08:30)" };
  const time = "time" in at ? at.time.trim().replace("T", " ") : "";
  switch (kind) {
    case "databricks":
      return { sql: "version" in at ? `SELECT *\nFROM ${q} VERSION AS OF ${Math.trunc(at.version)}\nLIMIT ${n}` : `SELECT *\nFROM ${q} TIMESTAMP AS OF '${time}'\nLIMIT ${n}` };
    case "snowflake":
      if ("version" in at) return { unavailable: "Snowflake reads a table as of a time, not a version" };
      return { sql: `SELECT *\nFROM ${q} AT(TIMESTAMP => '${time}'::TIMESTAMP_LTZ)\nLIMIT ${n}` };
    case "bigquery":
      if ("version" in at) return { unavailable: "BigQuery reads a table as of a time, not a version" };
      return { sql: `SELECT *\nFROM ${q} FOR SYSTEM_TIME AS OF TIMESTAMP '${time}'\nLIMIT ${n}` };
    default:
      return { unavailable: `${traitsOf(kind).name} doesn't keep earlier versions of a table's rows` };
  }
}

/** Rows written, from Delta's operation metrics (`numOutputRows`), as the database printed the map. */
export const rowsWritten = (metrics: string | null) => {
  const m = metrics?.match(/numOutputRows\W+(\d+)/);
  return m ? Number(m[1]) : null;
};
