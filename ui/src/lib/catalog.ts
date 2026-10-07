// Pure helpers for the catalog tree and catalog search (unit tested).
import type { CatalogInfo, ConnectorKind, DbObject, ObjectKind, SchemaInfo } from "./types";
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

const isRelation = (k: ObjectKind) => k !== "function" && k !== "procedure" && k !== "package" && k !== "sequence";

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

/** Tables and views (have columns, can be selected from). */
export function isRelationKind(kind: ObjectKind): boolean {
  return kind === "table" || kind === "view" || kind === "materialized_view" || kind === "foreign_table";
}

/** Explorer folders under a schema, in display order. */
export const OBJECT_GROUPS = ["Tables", "Views", "Materialized views", "Functions", "Procedures", "Packages", "Sequences", "Other"] as const;
export type ObjectGroupName = (typeof OBJECT_GROUPS)[number];

export function groupOf(kind: ObjectKind): ObjectGroupName {
  switch (kind) {
    case "table":
    case "foreign_table":
      return "Tables";
    case "view":
      return "Views";
    case "materialized_view":
      return "Materialized views";
    case "function":
      return "Functions";
    case "procedure":
      return "Procedures";
    case "package":
      return "Packages";
    case "sequence":
      return "Sequences";
    default:
      return "Other";
  }
}

/** Objects grouped into explorer folders (empty folders left out). */
export function groupObjects(objects: DbObject[]): [ObjectGroupName, DbObject[]][] {
  const g = new Map<ObjectGroupName, DbObject[]>(OBJECT_GROUPS.map((n) => [n, []]));
  for (const o of objects) g.get(groupOf(o.kind))!.push(o);
  return [...g].filter(([, v]) => v.length > 0);
}

/** Folders that start open: things you query. Routines and sequences start closed. */
export function groupOpenByDefault(name: ObjectGroupName): boolean {
  return name === "Tables" || name === "Views" || name === "Materialized views";
}

/** Rows rendered per folder before "Show more" (keeps 10k-table schemas fast). */
export const GROUP_PAGE = 200;

/**
 * Visible slice of a folder: items matching `filter` (substring, any case),
 * the first `limit` of them, plus `keep` (an item revealed by search) when
 * it falls outside the slice.
 */
export function pageObjects(items: DbObject[], filter: string, limit: number, keep?: (o: DbObject) => boolean): { shown: DbObject[]; matched: number } {
  const f = filter.trim().toLowerCase();
  const matched = f ? items.filter((o) => o.name.toLowerCase().includes(f)) : items;
  const shown = matched.slice(0, limit);
  if (keep && !shown.some(keep)) {
    const k = matched.find(keep) ?? items.find(keep);
    if (k) shown.push(k);
  }
  return { shown, matched: matched.length };
}

// ------------------------------------------------------------------ schema filter

/** Top-level items the explorer filter chooses from: catalogs, or schemas on two-level engines. */
export function filterLevel(schemas: SchemaInfo[]): { kind: "catalog" | "schema"; items: { name: string; count: number; isDefault: boolean }[] } {
  const groups = groupSchemas(schemas);
  if (groups) return { kind: "catalog", items: groups.map((g) => ({ name: g.name, count: g.schemas.length, isDefault: g.isDefault })) };
  return { kind: "schema", items: schemas.map((s) => ({ name: s.name, count: 0, isDefault: s.is_default })) };
}

/** Saved filter as top-level names (older filters listed `catalog.schema` ids). */
export function filterNames(schemas: SchemaInfo[], chosen: string[] | undefined): Set<string> {
  const out = new Set<string>();
  if (!chosen) return out;
  const byName = new Map(schemas.map((s) => [s.name, s]));
  const catalogs = !!groupSchemas(schemas);
  for (const c of chosen) {
    const s = byName.get(c);
    out.add(catalogs && s?.catalog ? s.catalog : c);
  }
  return out;
}

/** Noun for the explorer's top level of a connection kind. */
export function topLevelNoun(kind: ConnectorKind, plural = false): string {
  const n = kind === "bigquery" ? "project" : kind === "databricks" ? "catalog" : THREE_LEVEL.includes(kind) ? "database" : "schema";
  return plural ? `${n}s` : n;
}

/**
 * Schemas to list for a connection. The filter holds top-level names: catalogs
 * (databases/projects) on three-level engines, schemas elsewhere; a chosen
 * catalog lists all of its schemas. Unknown names are ignored.
 */
export function visibleSchemas(schemas: SchemaInfo[], chosen: string[] | undefined, also?: string | null): SchemaInfo[] {
  if (!chosen || chosen.length === 0) return schemas;
  const set = new Set(chosen);
  // A schema with an object revealed by search is shown even when not chosen.
  if (also) set.add(also);
  const out = schemas.filter((s) => set.has(s.name) || (!!s.catalog && set.has(s.catalog)));
  // Every chosen schema is gone (renamed/dropped): show all rather than nothing.
  return out.length ? out : schemas;
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
/** Explorer schema id under which DuckDB connections list DataBrain outputs. */
export const RESULTS_SCHEMA_ID = "results.main";

export function tablePath(kind: ConnectorKind, schema: string, name: string): string {
  if (kind === "duckdb" && schema.endsWith(".files")) return `files.${quoteIdent(kind, name)}`;
  if (kind === "duckdb" && schema === RESULTS_SCHEMA_ID) return `results.${quoteIdent(kind, name)}`;
  return qualifiedName(kind, schema, name);
}

/** Comma-separated quoted column list, one per line after the first few. */
export function columnList(kind: ConnectorKind, columns: string[], qualifier?: string): string {
  const q = (c: string) => (qualifier ? `${qualifier}.` : "") + quoteIdent(kind, c);
  if (columns.length <= 4) return columns.map(q).join(", ");
  return columns.map(q).join(",\n  ");
}

// ------------------------------------------------------------------ explorer cache / catalog-first

/**
 * Replace one catalog's schemas inside a connection's schema list (catalog
 * order kept as `catalogs` says; unknown catalogs last).
 */
export function mergeCatalogSchemas(all: SchemaInfo[] | undefined, catalog: string, list: SchemaInfo[], catalogs?: CatalogInfo[]): SchemaInfo[] {
  const rest = (all ?? []).filter((s) => s.catalog !== catalog);
  const merged = [...rest, ...list];
  if (!catalogs) return merged;
  const pos = new Map(catalogs.map((c, i) => [c.name, i]));
  const at = (s: SchemaInfo) => pos.get(s.catalog ?? "") ?? Number.MAX_SAFE_INTEGER;
  // Stable: schema order inside a catalog stays the server's.
  return merged.map((s, i) => [s, i] as const).sort((a, b) => at(a[0]) - at(b[0]) || a[1] - b[1]).map(([s]) => s);
}

/**
 * Catalogs to list for a connection. The filter holds catalog names (older
 * filters: `catalog.schema` ids, whose catalog counts); `also` = a schema id
 * revealed by search, whose catalog is shown too.
 */
export function visibleCatalogs(catalogs: CatalogInfo[], chosen: string[] | undefined, also?: string | null): CatalogInfo[] {
  if (!chosen || chosen.length === 0) return catalogs;
  const want = new Set(chosen.map((c) => (catalogs.some((k) => k.name === c) ? c : c.split(".")[0])));
  if (also) want.add(also.split(".")[0]);
  const out = catalogs.filter((c) => want.has(c.name));
  return out.length ? out : catalogs;
}

/**
 * Schemas whose tables are listed in the background when the explorer opens
 * (so clicking them shows at once): the default schema, the ones left open,
 * then the first few others, skipping what is loaded or cached already.
 */
export function prefetchSchemas(schemas: SchemaInfo[], open: Set<string>, have: Set<string>, max = 8): string[] {
  const order = [...schemas.filter((s) => s.is_default), ...schemas.filter((s) => !s.is_default && open.has(s.name)), ...schemas.filter((s) => !s.is_default && !open.has(s.name))];
  const out: string[] = [];
  for (const s of order) {
    if (out.length >= max) break;
    if (!have.has(s.name) && !out.includes(s.name)) out.push(s.name);
  }
  return out;
}

/** "cached 5 min ago" style age of a cache timestamp (ms). */
export function cacheAge(at: number | null | undefined, now = Date.now()): string {
  if (!at) return "never";
  const s = Math.max(0, Math.round((now - at) / 1000));
  if (s < 60) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h} h ago`;
  return `${Math.round(h / 24)} days ago`;
}
