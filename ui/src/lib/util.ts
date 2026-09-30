import type {
  ColumnFilter,
  ConnectorKind,
  FilterOp,
  PlannedStatement,
  SortKey,
  ViewSpec,
} from "./types";

export const emptyView = (): ViewSpec => ({ filters: [], quick_filter: null, sort: [] });

export function uid(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) return crypto.randomUUID();
  return Math.random().toString(36).slice(2) + Date.now().toString(36);
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
  const m = Math.floor(ms / 60_000);
  const s = Math.round((ms % 60_000) / 1000);
  return `${m}m ${s}s`;
}

export function formatCount(n: number): string {
  return n.toLocaleString("en-US");
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

export function relativeTime(ms: number, now = Date.now()): string {
  const d = Math.max(0, now - ms);
  if (d < 60_000) return "just now";
  if (d < 3_600_000) return `${Math.floor(d / 60_000)}m ago`;
  if (d < 86_400_000) return `${Math.floor(d / 3_600_000)}h ago`;
  return new Date(ms).toLocaleDateString();
}

/**
 * Header click sorting: toggles asc -> desc -> off for `column`. With
 * `additive` (shift-click) the key is added to/updated in the existing sort.
 */
export function toggleSort(sort: SortKey[], column: number, additive: boolean): SortKey[] {
  const existing = sort.find((s) => s.column === column);
  let next: SortKey | null;
  if (!existing) next = { column, descending: false };
  else if (!existing.descending) next = { column, descending: true };
  else next = null;

  if (!additive) return next ? [next] : [];
  const rest = sort.filter((s) => s.column !== column);
  if (!next) return rest;
  return existing ? sort.map((s) => (s.column === column ? next! : s)) : [...rest, next];
}

export function upsertFilter(filters: ColumnFilter[], f: ColumnFilter): ColumnFilter[] {
  const i = filters.findIndex((x) => x.column === f.column && x.op === f.op);
  if (i === -1) return [...filters, f];
  const copy = filters.slice();
  copy[i] = f;
  return copy;
}

export const FILTER_OPS: { op: FilterOp; label: string; needsValue: boolean }[] = [
  { op: "contains", label: "contains", needsValue: true },
  { op: "not_contains", label: "does not contain", needsValue: true },
  { op: "equals", label: "=", needsValue: true },
  { op: "not_equals", label: "≠", needsValue: true },
  { op: "starts_with", label: "starts with", needsValue: true },
  { op: "ends_with", label: "ends with", needsValue: true },
  { op: "gt", label: ">", needsValue: true },
  { op: "gte", label: "≥", needsValue: true },
  { op: "lt", label: "<", needsValue: true },
  { op: "lte", label: "≤", needsValue: true },
  { op: "is_null", label: "is NULL", needsValue: false },
  { op: "is_not_null", label: "is not NULL", needsValue: false },
];

export function filterLabel(f: ColumnFilter, columnName: string): string {
  const op = FILTER_OPS.find((o) => o.op === f.op);
  return op?.needsValue ? `${columnName} ${op.label} ${f.value}` : `${columnName} ${op?.label ?? f.op}`;
}

/**
 * Editor range to underline for an error. `position` is the 1-based
 * character position reported by the server within the statement.
 */
export function errorRange(
  stmt: Pick<PlannedStatement, "start" | "end" | "sql">,
  position: number | undefined,
): { from: number; to: number } {
  if (!position || position < 1) return { from: stmt.start, to: stmt.end };
  // Position counts code points; editor offsets are UTF-16 units.
  const chars = Array.from(stmt.sql);
  const before = chars.slice(0, position - 1).join("");
  const from = stmt.start + before.length;
  const rest = chars.slice(position - 1).join("");
  const m = rest.match(/^[^\s,;()]+/);
  const len = m ? m[0].length : Math.min(1, rest.length);
  return { from, to: Math.min(stmt.end, from + Math.max(1, len)) };
}

const UPPER_FOLDING: ConnectorKind[] = ["oracle", "snowflake"];

export function quoteIdent(kind: ConnectorKind, ident: string): string {
  // Unquoted identifiers fold to upper case in Oracle/Snowflake, lower elsewhere.
  const safe = UPPER_FOLDING.includes(kind) ? /^[A-Z_][A-Z0-9_$]*$/ : /^[a-z_][a-z0-9_]*$/;
  if (safe.test(ident)) return ident;
  switch (kind) {
    case "mysql":
    case "databricks":
    case "bigquery":
      return "`" + ident.replace(/`/g, "``") + "`";
    case "mssql":
      return "[" + ident.replace(/]/g, "]]") + "]";
    default:
      return '"' + ident.replace(/"/g, '""') + '"';
  }
}

/** Qualified name. Cloud/DuckDB schema names are `catalog.schema` paths. */
export function qualifiedName(kind: ConnectorKind, schema: string, name: string): string {
  const parts = ["snowflake", "databricks", "bigquery", "duckdb"].includes(kind) ? schema.split(".") : [schema];
  if (kind === "bigquery") return "`" + [...parts, name].join(".").replace(/`/g, "") + "`";
  return [...parts, name].map((p) => quoteIdent(kind, p)).join(".");
}

export function selectTopSql(kind: ConnectorKind, schema: string, table: string, n = 100): string {
  const q = qualifiedName(kind, schema, table);
  if (kind === "mssql") return `SELECT TOP ${n} *\nFROM ${q};`;
  if (kind === "oracle") return `SELECT *\nFROM ${q}\nFETCH FIRST ${n} ROWS ONLY`;
  return `SELECT *\nFROM ${q}\nLIMIT ${n};`;
}

/** First line of SQL, trimmed, for titles and list items. */
export function sqlPreview(sql: string, max = 80): string {
  const one = sql.replace(/\s+/g, " ").trim();
  return one.length > max ? one.slice(0, max - 1) + "…" : one;
}

/** Simple fuzzy match: all query characters appear in order. */
export function fuzzyMatch(query: string, text: string): boolean {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  let i = 0;
  for (const c of t) {
    if (c === q[i]) i++;
    if (i === q.length) return true;
  }
  return q.length === 0;
}

export type DiffLine = { op: "same" | "add" | "del"; text: string };

/** Line diff (LCS). Inputs over ~2,000 lines fall back to delete-all/add-all. */
export function lineDiff(before: string, after: string): DiffLine[] {
  const a = before.split("\n");
  const b = after.split("\n");
  if (a.length * b.length > 4_000_000) {
    return [...a.map((text) => ({ op: "del" as const, text })), ...b.map((text) => ({ op: "add" as const, text }))];
  }
  const m = a.length;
  const n = b.length;
  const dp = Array.from({ length: m + 1 }, () => new Uint32Array(n + 1));
  for (let i = m - 1; i >= 0; i--)
    for (let j = n - 1; j >= 0; j--) dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const out: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < m && j < n) {
    if (a[i] === b[j]) {
      out.push({ op: "same", text: a[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) out.push({ op: "del", text: a[i++] });
    else out.push({ op: "add", text: b[j++] });
  }
  while (i < m) out.push({ op: "del", text: a[i++] });
  while (j < n) out.push({ op: "add", text: b[j++] });
  return out;
}

/**
 * Pages (of `pageSize` rows) covering a visible row window. The grid can
 * report negative or non-integer positions while it is resized, so the range
 * is clamped to [0, last page]. `totalRows` of 0 (unknown yet) still yields page 0.
 */
export function pageRange(y: number, height: number, pageSize: number, totalRows: number): number[] {
  const lastPage = Math.max(0, Math.ceil(totalRows / pageSize) - 1);
  const y0 = Number.isFinite(y) ? y : 0;
  const top = Math.max(0, y0);
  const bottom = Math.max(top, y0 + (Number.isFinite(height) ? Math.max(0, height) : 0));
  const first = Math.min(lastPage, Math.floor(top / pageSize));
  const last = Math.min(lastPage, Math.floor(bottom / pageSize));
  const out: number[] = [];
  for (let p = first; p <= last; p++) out.push(p);
  return out;
}
