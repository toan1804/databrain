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
function split(kind: ConnectorKind, schema: string): [string | null, string] {
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

