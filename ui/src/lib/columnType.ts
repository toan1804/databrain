import type { ColumnMeta, ConnectorKind, OutputInfo } from "./types";

export const ENGINE_NAMES: Record<ConnectorKind, string> = {
  sqlite: "SQLite",
  postgres: "PostgreSQL",
  mysql: "MySQL",
  mssql: "SQL Server",
  oracle: "Oracle",
  snowflake: "Snowflake",
  databricks: "Databricks",
  bigquery: "BigQuery",
  duckdb: "DuckDB",
};

export interface ColumnTypeInfo {
  name: string;
  /** Type as the database that produced the result names it. */
  type: string;
  /** Engine that ran the query (e.g. "postgres"), when known. */
  engine: string | null;
  /** DuckDB queries over outputs: the column's type in the source output. */
  sources: { ref: string; connection: string; engine: string; type: string }[];
}

const REF = /\bresults\.(?:"([^"]+)"|([A-Za-z_][\w]*))/gi;

/** Outputs a DuckDB query reads as `results.<handle or name>`. */
export function referencedOutputs(sql: string, outputs: OutputInfo[]): OutputInfo[] {
  const seen = new Set<string>();
  const out: OutputInfo[] = [];
  for (const m of sql.matchAll(REF)) {
    const want = (m[1] ?? m[2]).toLowerCase();
    const o = outputs.find((x) => x.handle.toLowerCase() === want) ?? outputs.find((x) => x.name?.toLowerCase() === want && x.active);
    if (o && !seen.has(o.result_id)) {
      seen.add(o.result_id);
      out.push(o);
    }
  }
  return out;
}

/** What the header tooltip of column `index` of result `resultId` shows. */
export function columnTypeInfo(col: ColumnMeta, resultId: string, outputs: OutputInfo[]): ColumnTypeInfo {
  const own = outputs.find((o) => o.result_id === resultId);
  const info: ColumnTypeInfo = { name: col.name, type: col.db_type || col.data_type, engine: own?.kind ?? null, sources: [] };
  // Results (DuckDB) over outputs: also the type the column has where it came from.
  if (own?.kind === "duckdb" && /\bresults\./i.test(own.sql)) {
    for (const src of referencedOutputs(own.sql, outputs)) {
      if (src.result_id === resultId) continue;
      const c = src.columns.find((x) => x.name === col.name) ?? src.columns.find((x) => x.name.toLowerCase() === col.name.toLowerCase());
      if (c) info.sources.push({ ref: `results.${src.name ?? src.handle}`, connection: src.connection_name, engine: src.kind, type: c.db_type || c.data_type });
    }
  }
  return info;
}
