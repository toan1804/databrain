// Pure helpers for the catalog tree and catalog search (unit tested).
import type { ConnectorKind, DbObject, ObjectKind, SchemaInfo } from "./types";
import { qualifiedName, quoteIdent } from "./util";

/** Engines whose schema ids are `catalog.schema` (three-level names). */
export const THREE_LEVEL: ConnectorKind[] = ["databricks", "snowflake", "bigquery", "duckdb"];

export interface CatalogGroup {
  name: string;
  schemas: SchemaInfo[];
  /** Contains the session's default schema. */
  isDefault: boolean;
}

/**
 * Group schemas under their catalog (Databricks catalog, Snowflake database,
 * BigQuery project, DuckDB database). Returns null for two-level engines.
 * Catalog order follows the server's (default first); schemas inside a
 * catalog are default-first, then alphabetical.
 */
export function groupSchemas(schemas: SchemaInfo[]): CatalogGroup[] | null {
  if (!schemas.some((s) => s.catalog)) return null;
  const map = new Map<string, CatalogGroup>();
  for (const s of schemas) {
    const name = s.catalog ?? "";
    let g = map.get(name);
    if (!g) map.set(name, (g = { name, schemas: [], isDefault: false }));
    g.schemas.push(s);
    if (s.is_default) g.isDefault = true;
  }
  const groups = [...map.values()];
  for (const g of groups) {
    g.schemas.sort((a, b) => Number(b.is_default) - Number(a.is_default) || schemaLabel(a).localeCompare(schemaLabel(b)));
  }
  groups.sort((a, b) => Number(b.isDefault) - Number(a.isDefault));
  return groups;
}

/** Schema name without its catalog prefix. */
export function schemaLabel(s: { name: string; catalog?: string | null }): string {
  return s.catalog && s.name.startsWith(s.catalog + ".") ? s.name.slice(s.catalog.length + 1) : s.name;
}

/** Split a schema id into catalog + schema for display. */
export function splitSchema(kind: ConnectorKind, schema: string, known?: SchemaInfo[]): { catalog?: string; schema: string } {
  const hit = known?.find((s) => s.name === schema);
  if (hit) return { catalog: hit.catalog ?? undefined, schema: schemaLabel(hit) };
  if (THREE_LEVEL.includes(kind)) {
    const i = schema.indexOf(".");
    if (i > 0) return { catalog: schema.slice(0, i), schema: schema.slice(i + 1) };
  }
  return { schema };
}

/**
 * Same rule as the backend: case-insensitive substring of the name, or of
 * `schema.name` when the query is qualified (`sales.ord`).
 */
export function objectMatches(query: string, schema: string, name: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return false;
  const n = name.toLowerCase();
  if (!q.includes(".")) return n.includes(q);
  return `${schema.toLowerCase()}.${n}`.includes(q);
}

/** Part of the query matched against the object name. */
export function nameTerm(query: string): string {
  const parts = query.trim().split(".");
  return parts[parts.length - 1] ?? "";
}

const isRelation = (k: ObjectKind) => k !== "function" && k !== "procedure" && k !== "sequence";

/** Exact name, then prefix, then shorter names; de-duplicated and capped. */
export function rankHits(query: string, hits: DbObject[], limit = 100): DbObject[] {
  const term = nameTerm(query).toLowerCase();
  const tier = (o: DbObject) => {
    const n = o.name.toLowerCase();
    return n === term ? 0 : n.startsWith(term) ? 1 : 2;
  };
  const seen = new Set<string>();
  return hits
    .filter((o) => isRelation(o.kind))
    .filter((o) => {
      const k = `${o.schema}\u0000${o.name}`;
      if (seen.has(k)) return false;
      seen.add(k);
      return true;
    })
    .sort(
      (a, b) =>
        tier(a) - tier(b) || a.name.length - b.name.length || a.schema.localeCompare(b.schema) || a.name.localeCompare(b.name),
    )
    .slice(0, limit);
}

/** Matches already in the explorer cache (`objects` keyed `${conn}|${schema}`). */
export function cachedHits(query: string, connId: string, objects: Record<string, DbObject[]>): DbObject[] {
  const out: DbObject[] = [];
  const prefix = connId + "|";
  for (const [k, objs] of Object.entries(objects)) {
    if (!k.startsWith(prefix)) continue;
    for (const o of objs) if (objectMatches(query, o.schema, o.name)) out.push(o);
  }
  return out;
}

/** [start, end) of the name term inside `name`, for highlighting. */
export function matchRange(query: string, name: string): [number, number] | null {
  const t = nameTerm(query).toLowerCase();
  if (!t) return null;
  const i = name.toLowerCase().indexOf(t);
  return i < 0 ? null : [i, i + t.length];
}

export type ObjectGroupName = "Tables" | "Views" | "Routines";

export function groupOf(kind: ObjectKind): ObjectGroupName {
  if (kind === "table" || kind === "foreign_table") return "Tables";
  if (kind === "view" || kind === "materialized_view") return "Views";
  return "Routines";
}

// Tree expansion keys (kept in the store so search can reveal an object).
export const treeKey = {
  conn: (c: string) => `c|${c}`,
  catalog: (c: string, cat: string) => `k|${c}|${cat}`,
  schema: (c: string, s: string) => `s|${c}|${s}`,
  group: (c: string, s: string, g: string) => `g|${c}|${s}|${g}`,
  object: (c: string, s: string, n: string) => `o|${c}|${s}|${n}`,
};

/** Keys to open so that `obj` becomes visible in the tree. */
export function revealKeys(connId: string, obj: DbObject, catalog?: string): string[] {
  const keys = [treeKey.conn(connId), treeKey.schema(connId, obj.schema), treeKey.group(connId, obj.schema, groupOf(obj.kind))];
  if (catalog) keys.push(treeKey.catalog(connId, catalog));
  return keys;
}

// ------------------------------------------------------------------ copy / insert names

/** Quoted schema path (`catalog.schema` parts quoted per dialect). */
export function schemaPath(kind: ConnectorKind, schema: string): string {
  if (kind === "bigquery") return "`" + schema.replace(/`/g, "") + "`";
  const parts = THREE_LEVEL.includes(kind) ? splitIdent(schema) : [schema];
  return parts.map((p) => quoteIdent(kind, p)).join(".");
}

/** Split `a.b` into parts (catalog names never contain dots in these engines). */
function splitIdent(schema: string): string[] {
  const i = schema.indexOf(".");
  return i > 0 ? [schema.slice(0, i), schema.slice(i + 1)] : [schema];
}

/** Fully qualified, quoted table path, e.g. `main.sales.orders`. */
export function tablePath(kind: ConnectorKind, schema: string, name: string): string {
  if (kind === "duckdb" && schema.endsWith(".files")) return `files.${quoteIdent(kind, name)}`;
  return qualifiedName(kind, schema, name);
}

/** Comma-separated quoted column list, one per line after the first few. */
export function columnList(kind: ConnectorKind, columns: string[], qualifier?: string): string {
  const q = (c: string) => (qualifier ? `${qualifier}.` : "") + quoteIdent(kind, c);
  if (columns.length <= 4) return columns.map(q).join(", ");
  return columns.map(q).join(",\n  ");
}
