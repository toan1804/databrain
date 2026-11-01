// Saved grid views and per-table quick filters. A view keeps columns by
// name (results of the same query can come back with columns in another
// order, or some missing), and is mapped onto a result's columns when applied.
import type { ColumnFilter, FilterOp, ResultSource, SortKey, ViewSpec } from "./types";

export interface SavedView {
  id: string;
  name: string;
  /** Table key (`tableKey`) the view is for; null: any result with its columns. */
  table: string | null;
  filters: { column: string; op: FilterOp; value: string }[];
  sort: { column: string; descending: boolean }[];
  quick: string | null;
  at: number;
}

/** A remembered quick filter: its text and when it was last used. */
export interface QuickFilterEntry {
  q: string;
  at: number;
}

export const MAX_VIEWS = 200;
export const MAX_QUICK_FILTERS = 300;

/**
 * `connection|schema|table`, lower case. An unqualified table is in the
 * connection's default schema (`defaultSchema`), so `orders` and
 * `public.orders` share their settings.
 */
export function tableKey(src: Pick<ResultSource, "connection_id" | "schema" | "table">, defaultSchema?: string | null): string {
  return `${src.connection_id}|${(src.schema ?? defaultSchema ?? "").toLowerCase()}|${src.table.toLowerCase()}`;
}

/** The table part of a key, for labels (`sales.orders`). */
export function tableLabel(key: string): string {
  const [, schema, table] = key.split("|");
  return schema ? `${schema}.${table}` : (table ?? key);
}

/** The view as it is saved (columns by name). */
export function toSaved(view: ViewSpec, columns: string[], name: string, table: string | null, id: string, at = Date.now()): SavedView {
  const col = (i: number) => columns[i];
  return {
    id,
    name: name.trim(),
    table,
    filters: view.filters.filter((f) => col(f.column) !== undefined).map((f) => ({ column: col(f.column), op: f.op, value: f.value })),
    sort: view.sort.filter((s) => col(s.column) !== undefined).map((s) => ({ column: col(s.column), descending: s.descending })),
    quick: view.quick_filter?.trim() || null,
    at,
  };
}

/** Index of a column by name: exact, else the only case-insensitive match. */
function indexOf(columns: string[], name: string): number {
  const i = columns.indexOf(name);
  if (i >= 0) return i;
  const lower = name.toLowerCase();
  const hits = columns.map((c, j) => (c.toLowerCase() === lower ? j : -1)).filter((j) => j >= 0);
  return hits.length === 1 ? hits[0] : -1;
}

/** The saved view on these columns, and the column names it uses that the result doesn't have. */
export function applySaved(v: SavedView, columns: string[]): { view: ViewSpec; missing: string[] } {
  const missing = new Set<string>();
  const filters: ColumnFilter[] = [];
  for (const f of v.filters) {
    const i = indexOf(columns, f.column);
    if (i < 0) missing.add(f.column);
    else filters.push({ column: i, op: f.op, value: f.value });
  }
  const sort: SortKey[] = [];
  for (const s of v.sort) {
    const i = indexOf(columns, s.column);
    if (i < 0) missing.add(s.column);
    else if (!sort.some((k) => k.column === i)) sort.push({ column: i, descending: s.descending });
  }
  return { view: { filters, sort, quick_filter: v.quick }, missing: [...missing] };
}

/** Views offered for a result: those for its table, then those for any result whose columns they all find. */
export function viewsFor(views: SavedView[], table: string | null, columns: string[]): SavedView[] {
  const fits = (v: SavedView) => [...v.filters, ...v.sort].every((x) => indexOf(columns, x.column) >= 0);
  const mine = views.filter((v) => v.table !== null && v.table === table);
  const any = views.filter((v) => v.table === null && fits(v));
  const byName = (a: SavedView, b: SavedView) => a.name.localeCompare(b.name);
  return [...mine.sort(byName), ...any.sort(byName)];
}

/** Whether a view would change nothing (not worth saving). */
export function isEmptyView(view: ViewSpec): boolean {
  return !view.filters.length && !view.sort.length && !view.quick_filter?.trim();
}

const OPS: FilterOp[] = ["contains", "not_contains", "equals", "not_equals", "starts_with", "ends_with", "gt", "gte", "lt", "lte", "is_null", "is_not_null"];

/** `grid_views` setting (bad entries dropped, newest kept when over the cap). */
export function parseViews(v: unknown): SavedView[] {
  if (!Array.isArray(v)) return [];
  const str = (x: unknown) => typeof x === "string";
  const out: SavedView[] = [];
  for (const x of v) {
    if (!x || typeof x !== "object") continue;
    const o = x as Record<string, unknown>;
    if (!str(o.id) || !str(o.name) || !(o.name as string).trim()) continue;
    const filters = Array.isArray(o.filters)
      ? (o.filters as Record<string, unknown>[]).filter((f) => f && str(f.column) && OPS.includes(f.op as FilterOp)).map((f) => ({ column: f.column as string, op: f.op as FilterOp, value: str(f.value) ? (f.value as string) : "" }))
      : [];
    const sort = Array.isArray(o.sort) ? (o.sort as Record<string, unknown>[]).filter((s) => s && str(s.column)).map((s) => ({ column: s.column as string, descending: s.descending === true })) : [];
    out.push({ id: o.id as string, name: (o.name as string).trim(), table: str(o.table) ? (o.table as string) : null, filters, sort, quick: str(o.quick) && (o.quick as string).trim() ? (o.quick as string) : null, at: typeof o.at === "number" ? o.at : 0 });
  }
  return out.sort((a, b) => b.at - a.at).slice(0, MAX_VIEWS);
}

/** `grid_quick_filters` setting: { tableKey: { q, at } }. */
export function parseQuickFilters(v: unknown): Record<string, QuickFilterEntry> {
  if (!v || typeof v !== "object" || Array.isArray(v)) return {};
  const entries = Object.entries(v as Record<string, unknown>)
    .filter(([, e]) => !!e && typeof e === "object" && typeof (e as QuickFilterEntry).q === "string" && (e as QuickFilterEntry).q.trim() !== "")
    .map(([k, e]) => [k, { q: (e as QuickFilterEntry).q, at: typeof (e as QuickFilterEntry).at === "number" ? (e as QuickFilterEntry).at : 0 }] as const);
  return Object.fromEntries(entries.sort((a, b) => b[1].at - a[1].at).slice(0, MAX_QUICK_FILTERS));
}

/** Remember (or forget, for an empty text) a table's quick filter; the oldest go over the cap. */
export function rememberQuickFilter(all: Record<string, QuickFilterEntry>, key: string, q: string, at = Date.now()): Record<string, QuickFilterEntry> {
  const next = { ...all };
  if (q.trim()) next[key] = { q, at };
  else delete next[key];
  return parseQuickFilters(next);
}
