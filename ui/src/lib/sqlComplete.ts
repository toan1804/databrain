// Context-aware SQL completion: keywords for where the cursor is, tables of
// the connection after FROM/JOIN, columns of the tables in the statement,
// and `schema.` / `alias.` paths. Pure logic over a metadata provider (tested
// without CodeMirror); see components/sqlAssist.ts for the editor adapter.
import type { ConnectorKind, DbObject, SchemaInfo } from "./types";
import { THREE_LEVEL, schemaLabel } from "./catalog";
import { quoteIdent } from "./util";

export interface MetaProvider {
  kind: ConnectorKind;
  /** Known schemas (undefined while not loaded). */
  schemas(): SchemaInfo[] | undefined;
  /** Tables/views of a schema matching `typed` (may search the server; never loads a whole big schema). */
  objects(schema: string, typed: string): Promise<DbObject[] | undefined>;
  /** Already-loaded objects of every schema (no I/O). */
  cachedObjects(): DbObject[];
  /** Already-loaded objects named `name` (any case); faster than scanning cachedObjects on big catalogs. */
  cachedNamed?(name: string): DbObject[];
  /** Table search by name: local index + server (may be empty when offline). */
  searchTables(prefix: string): Promise<DbObject[]>;
  /** Column names of a table (may load). */
  columns(schema: string, table: string): Promise<string[] | undefined>;
  /** Key roles of a table's columns (`partition key`, `indexed`…), when known (no I/O). */
  columnRoles?(schema: string, table: string): Record<string, string> | undefined;
  /** Extra virtual tables, e.g. DuckDB `results.<output>`. */
  virtualTables?(): { schema: string; name: string; columns: string[] }[];
}

export type OptionType = "keyword" | "function" | "table" | "view" | "column" | "schema" | "catalog" | "alias";

export interface SqlOption {
  label: string;
  type: OptionType;
  detail?: string;
  /** Text to insert (defaults to `label`). */
  apply?: string;
  /** Re-open completion after inserting (namespaces ending in "."). */
  reopen?: boolean;
  boost?: number;
}

export interface SqlCompletion {
  from: number;
  options: SqlOption[];
}

// ------------------------------------------------------------------ vocabulary

const STATEMENT_START = ["SELECT", "WITH", "INSERT INTO", "UPDATE", "DELETE FROM", "CREATE TABLE", "CREATE VIEW", "ALTER TABLE", "DROP TABLE", "EXPLAIN", "SHOW", "DESCRIBE", "USE"];
const AFTER_TABLE = ["WHERE", "JOIN", "LEFT JOIN", "INNER JOIN", "RIGHT JOIN", "FULL JOIN", "CROSS JOIN", "ON", "AS", "GROUP BY", "ORDER BY", "LIMIT", "UNION", "UNION ALL", "HAVING", "USING", "SET", "VALUES"];
const IN_EXPRESSION = [
  "FROM", "AS", "AND", "OR", "NOT", "NULL", "IS NULL", "IS NOT NULL", "IN", "LIKE", "ILIKE", "BETWEEN", "CASE", "WHEN", "THEN", "ELSE", "END",
  "DISTINCT", "TRUE", "FALSE", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "ASC", "DESC", "OVER", "PARTITION BY", "EXISTS", "WHERE", "JOIN",
];
const FUNCTIONS = [
  "COUNT", "SUM", "AVG", "MIN", "MAX", "COALESCE", "NULLIF", "CAST", "ROUND", "ABS", "LOWER", "UPPER", "TRIM", "LENGTH", "SUBSTRING", "CONCAT",
  "REPLACE", "DATE_TRUNC", "EXTRACT", "CURRENT_DATE", "CURRENT_TIMESTAMP", "NOW", "ROW_NUMBER", "RANK", "DENSE_RANK", "LAG", "LEAD",
  "FIRST_VALUE", "LAST_VALUE", "STRING_AGG", "ARRAY_AGG", "GREATEST", "LEAST", "IFNULL",
];
const DIALECT_FUNCTIONS: Partial<Record<ConnectorKind, string[]>> = {
  mssql: ["ISNULL", "GETDATE", "DATEADD", "DATEDIFF", "FORMAT", "TOP", "STRING_AGG"],
  mysql: ["IFNULL", "DATE_FORMAT", "STR_TO_DATE", "GROUP_CONCAT", "DATEDIFF"],
  oracle: ["NVL", "TO_CHAR", "TO_DATE", "SYSDATE", "LISTAGG", "DECODE"],
  snowflake: ["IFF", "NVL", "TO_DATE", "TO_CHAR", "DATEADD", "DATEDIFF", "LISTAGG", "TRY_CAST", "QUALIFY"],
  databricks: ["IFF", "NVL", "TO_DATE", "DATE_FORMAT", "DATEDIFF", "COLLECT_LIST", "TRY_CAST", "QUALIFY"],
  bigquery: ["SAFE_CAST", "FORMAT_DATE", "DATE_DIFF", "TIMESTAMP_TRUNC", "IFNULL", "QUALIFY"],
  duckdb: ["READ_CSV", "READ_PARQUET", "READ_JSON", "LIST", "STRFTIME", "DATEDIFF", "TRY_CAST", "QUALIFY"],
  postgres: ["TO_CHAR", "TO_DATE", "AGE", "GENERATE_SERIES", "JSONB_EXTRACT_PATH_TEXT"],
};

const TABLE_KEYWORDS = new Set(["from", "join", "into", "update", "table"]);
const EXPR_KEYWORDS = new Set(["select", "where", "on", "and", "or", "by", "having", "set", "when", "then", "else", "case", "distinct", "not", "in", "like", "ilike", "between", "is", "over", "using", "returning", "values", "qualify"]);

// ------------------------------------------------------------------ lexing

/** Replace strings and comments with spaces (offsets preserved). */
export function blankLiterals(sql: string): string {
  let out = "";
  let i = 0;
  while (i < sql.length) {
    const c = sql[i];
    const n = sql[i + 1];
    if (c === "-" && n === "-") {
      const j = sql.indexOf("\n", i);
      const end = j < 0 ? sql.length : j;
      out += " ".repeat(end - i);
      i = end;
    } else if (c === "/" && n === "*") {
      const j = sql.indexOf("*/", i + 2);
      const end = j < 0 ? sql.length : j + 2;
      out += sql.slice(i, end).replace(/[^\n]/g, " ");
      i = end;
    } else if (c === "'") {
      let j = i + 1;
      while (j < sql.length && !(sql[j] === "'" && sql[j + 1] !== "'")) j += sql[j] === "'" ? 2 : 1;
      const end = Math.min(sql.length, j + 1);
      out += " ".repeat(end - i);
      i = end;
    } else {
      out += c;
      i++;
    }
  }
  return out;
}

/** [start, end) of the statement around `pos` (split on `;`). */
export function statementBounds(clean: string, pos: number): [number, number] {
  const start = clean.lastIndexOf(";", pos - 1) + 1;
  const e = clean.indexOf(";", pos);
  return [start, e < 0 ? clean.length : e];
}

const IDENT = String.raw`(?:[A-Za-z_][\w$]*|"(?:[^"]|"")*"|\x60[^\x60]*\x60|\[[^\]]*\])`;
const PATH_RE = new RegExp(`^(?:${IDENT}\\.)*(?:[A-Za-z_][\\w$]*|"[^"]*|\\x60[^\\x60]*|\\[[^\\]]*)?$`);

export function unquote(part: string): string {
  if (part.length >= 2 && ((part[0] === '"' && part.endsWith('"')) || (part[0] === "`" && part.endsWith("`")) || (part[0] === "[" && part.endsWith("]")))) {
    return part.slice(1, -1).replace(/""/g, '"');
  }
  return part.replace(/^["`[]/, "");
}

/** Split a dotted path into parts, respecting quotes. */
export function splitPath(path: string): string[] {
  const parts: string[] = [];
  let cur = "";
  let q: string | null = null;
  for (const ch of path) {
    if (q) {
      cur += ch;
      if (ch === q) q = null;
    } else if (ch === '"' || ch === "`") {
      q = ch;
      cur += ch;
    } else if (ch === "[") {
      q = "]";
      cur += ch;
    } else if (ch === ".") {
      parts.push(cur);
      cur = "";
    } else cur += ch;
  }
  parts.push(cur);
  return parts;
}

export interface TableRef {
  /** Dotted reference as written, unquoted parts. */
  parts: string[];
  alias?: string;
}

const NOT_ALIAS = new Set([
  "where", "join", "left", "right", "inner", "outer", "full", "cross", "on", "group", "order", "limit", "union", "having", "using", "set", "values",
  "natural", "lateral", "window", "qualify", "as", "select", "from", "returning", "pivot", "unpivot", "tablesample", "with", "into",
]);

/** Tables referenced in a statement (`FROM a x, b AS y JOIN c ON …`). */
export function tableRefs(stmt: string): TableRef[] {
  const out: TableRef[] = [];
  const re = new RegExp(String.raw`\b(from|join|update|into)\s+((?:${IDENT}\s*\.\s*)*${IDENT})(?:\s+(?:as\s+)?(${IDENT}))?`, "gi");
  let m: RegExpExecArray | null;
  while ((m = re.exec(stmt))) {
    const push = (path: string, alias?: string) => {
      const parts = splitPath(path.replace(/\s*\.\s*/g, ".").trim()).map(unquote);
      if (!parts.length || !parts[parts.length - 1] || parts[0].startsWith("(")) return;
      const a = alias && !NOT_ALIAS.has(alias.toLowerCase()) ? unquote(alias) : undefined;
      out.push({ parts, alias: a });
    };
    push(m[2], m[3]);
    // Comma-separated FROM lists: FROM a x, b y
    if (m[1].toLowerCase() === "from") {
      const rest = stmt.slice(m.index + m[0].length);
      const more = new RegExp(String.raw`^\s*,\s*((?:${IDENT}\s*\.\s*)*${IDENT})(?:\s+(?:as\s+)?(${IDENT}))?`, "i");
      let r = rest;
      let mm: RegExpExecArray | null;
      while ((mm = more.exec(r))) {
        push(mm[1], mm[2]);
        r = r.slice(mm[0].length);
      }
    }
  }
  return out;
}

/** Names defined as CTEs (`WITH name AS (`). */
export function cteNames(stmt: string): string[] {
  const out: string[] = [];
  const re = /(?:\bwith\s+(?:recursive\s+)?|,\s*)([A-Za-z_][\w$]*)\s+as\s*\(/gi;
  let m: RegExpExecArray | null;
  while ((m = re.exec(stmt))) out.push(m[1]);
  return out;
}

type Context = "start" | "table" | "after_table" | "expr";

/** Clause context from the text of the statement before the current word. */
export function clauseContext(before: string): Context {
  const toks = before.match(new RegExp(`${IDENT}|[(),;*=<>!+\\-/]|\\S`, "g")) ?? [];
  if (toks.length === 0) return "start";
  // Walk back over the current FROM list: ident(.ident)* [alias] ,
  let i = toks.length - 1;
  const last = toks[i].toLowerCase();
  if (TABLE_KEYWORDS.has(last)) return "table";
  if (last === ",") {
    // Comma inside a FROM list → table; otherwise expression list.
    for (let j = i - 1; j >= 0; j--) {
      const t = toks[j].toLowerCase();
      if (TABLE_KEYWORDS.has(t)) return "table";
      if (EXPR_KEYWORDS.has(t) || t === "(" || t === ")") return "expr";
    }
    return "expr";
  }
  if (EXPR_KEYWORDS.has(last) || /^[(=<>!+\-/*]$/.test(last)) return "expr";
  // After an identifier: in a FROM list it's a table name → next come
  // WHERE/JOIN/aliases; in an expression → operators/keywords.
  for (let j = i; j >= 0; j--) {
    const t = toks[j].toLowerCase();
    if (TABLE_KEYWORDS.has(t)) return "after_table";
    if (EXPR_KEYWORDS.has(t)) return "expr";
  }
  return "start";
}

// ------------------------------------------------------------------ matching

/** 0 = exact, 1 = prefix, 2 = word-part prefix (`id` → `customer_id`), -1 = no match. */
export function matchRank(label: string, typed: string): number {
  if (!typed) return 1;
  const l = label.toLowerCase();
  const t = typed.toLowerCase();
  if (l === t) return 0;
  if (l.startsWith(t)) return 1;
  if (t.length >= 2 && l.split(/[_\s]/).some((p, i) => i > 0 && p.startsWith(t))) return 2;
  return -1;
}

function filterRank(opts: SqlOption[], typed: string, limit = 200): SqlOption[] {
  const seen = new Map<string, { o: SqlOption; r: number; i: number }>();
  const scored: { o: SqlOption; r: number; i: number }[] = [];
  opts.forEach((o, i) => {
    // Same column in several tables: one entry listing the tables.
    const key = o.type === "column" ? `column\u0000${o.label.toLowerCase()}` : `${o.type}\u0000${o.label.toLowerCase()}\u0000${o.detail ?? ""}`;
    const prev = seen.get(key);
    if (prev) {
      if (o.type === "column" && o.detail && prev.o.detail && !prev.o.detail.split(", ").includes(o.detail)) prev.o = { ...prev.o, detail: `${prev.o.detail}, ${o.detail}` };
      return;
    }
    const r = matchRank(o.label, typed);
    if (r < 0) return;
    const entry = { o, r, i };
    seen.set(key, entry);
    scored.push(entry);
  });
  scored.sort((a, b) => a.r - b.r || (b.o.boost ?? 0) - (a.o.boost ?? 0) || a.i - b.i);
  return scored.slice(0, limit).map((s, k) => ({ ...s.o, boost: 1000 - k }));
}

/** Keywords in the case the user types (`sel` → select, `SEL` → SELECT). */
function kw(words: string[], typed: string, boost = 0, type: OptionType = "keyword"): SqlOption[] {
  const upper = typed.length > 0 && typed === typed.toUpperCase() && /[A-Z]/.test(typed);
  const lower = typed.length > 0 && !upper;
  return words.map((w) => ({ label: lower ? w.toLowerCase() : w, type, boost }));
}

// ------------------------------------------------------------------ resolution

function threeLevel(kind: ConnectorKind) {
  return THREE_LEVEL.includes(kind);
}

function defaultSchema(schemas: SchemaInfo[] | undefined): SchemaInfo | undefined {
  return schemas?.find((s) => s.is_default) ?? (schemas?.length === 1 ? schemas[0] : undefined);
}

/** Schema id for a written schema reference (label or `catalog.schema`). */
function findSchema(p: MetaProvider, written: string[]): SchemaInfo | undefined {
  const schemas = p.schemas() ?? [];
  const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();
  if (written.length === 2) return schemas.find((s) => eq(s.name, `${written[0]}.${written[1]}`));
  if (written.length !== 1) return undefined;
  const def = defaultSchema(schemas);
  const byLabel = schemas.filter((s) => eq(schemaLabel(s), written[0]) || eq(s.name, written[0]));
  return byLabel.find((s) => s.catalog && def?.catalog && s.catalog === def.catalog) ?? byLabel[0];
}

/** Resolve a table reference to (schema id, table) using known metadata. */
export async function resolveTable(p: MetaProvider, parts: string[]): Promise<{ schema: string; name: string } | undefined> {
  const name = parts[parts.length - 1];
  if (parts.length > 1) {
    const sc = findSchema(p, parts.slice(0, -1));
    if (sc) return { schema: sc.name, name };
    if (p.kind === "duckdb" && parts.length === 2) return { schema: parts[0], name }; // results.x, files.x
    return { schema: parts.slice(0, -1).join("."), name };
  }
  const cached = p.cachedNamed ? p.cachedNamed(name) : p.cachedObjects().filter((o) => o.name.toLowerCase() === name.toLowerCase());
  const def = defaultSchema(p.schemas());
  const inDef = cached.find((o) => o.schema === def?.name);
  if (inDef || cached[0]) return { schema: (inDef ?? cached[0]).schema, name: (inDef ?? cached[0]).name };
  if (def) return { schema: def.name, name };
  return undefined;
}

async function columnsFor(p: MetaProvider, parts: string[]): Promise<{ table: string; columns: string[]; roles?: Record<string, string> } | undefined> {
  const virt = p.virtualTables?.() ?? [];
  const last = parts[parts.length - 1].toLowerCase();
  const v = virt.find((t) => t.name.toLowerCase() === last && (parts.length === 1 || parts[parts.length - 2].toLowerCase() === t.schema.toLowerCase()));
  if (v) return { table: v.name, columns: v.columns };
  const r = await resolveTable(p, parts);
  if (!r) return undefined;
  const cols = await p.columns(r.schema, r.name);
  return cols ? { table: r.name, columns: cols, roles: p.columnRoles?.(r.schema, r.name) } : undefined;
}

/**
 * Reference text for a table, always schema-qualified so it runs as
 * inserted (`public.orders`, `files.order_items`, `crm.customers`). The
 * catalog is added when the table is outside the session's catalog.
 */
export function tableApply(p: MetaProvider, o: DbObject): string {
  const k = p.kind;
  const def = defaultSchema(p.schemas());
  const known = (p.schemas() ?? []).find((s) => s.name === o.schema);
  if (k === "duckdb" && o.schema.endsWith(".files")) return `files.${quoteIdent(k, o.name)}`;
  if (k === "duckdb" && o.schema === "results.main") return `results.${quoteIdent(k, o.name)}`;
  let parts: string[];
  if (threeLevel(k) && (known?.catalog || o.schema.includes("."))) {
    const cat = known?.catalog ?? o.schema.slice(0, o.schema.indexOf("."));
    const sch = known ? schemaLabel(known) : o.schema.slice(cat.length + 1);
    // Same catalog as the session: schema.table resolves (BigQuery needs the project).
    parts = cat === def?.catalog && k !== "bigquery" ? [sch] : [cat, sch];
  } else {
    parts = [o.schema];
  }
  if (k === "bigquery") return "`" + [...parts, o.name].join(".") + "`";
  return [...parts, o.name].map((x) => quoteIdent(k, x)).join(".");
}

const MAX_TABLE_OPTIONS = 300;

/** Tables/views whose name matches `typed` (see matchRank), best first, at most `limit`. */
export function matchingRelations(objects: DbObject[], typed: string, limit: number): DbObject[] {
  const buckets: DbObject[][] = [[], [], []];
  for (const o of objects) {
    if (o.kind === "function" || o.kind === "procedure" || o.kind === "sequence" || o.kind === "other") continue;
    const r = matchRank(o.name, typed);
    if (r >= 0) buckets[r].push(o);
  }
  const out: DbObject[] = [];
  for (const b of buckets) {
    for (const o of b) {
      if (out.length >= limit) return out;
      out.push(o);
    }
  }
  return out;
}

function tableOption(p: MetaProvider, o: DbObject, boost = 0): SqlOption {
  const def = defaultSchema(p.schemas());
  const sc = (p.schemas() ?? []).find((s) => s.name === o.schema);
  return {
    label: o.name,
    type: o.kind === "view" || o.kind === "materialized_view" ? "view" : "table",
    detail: o.schema === def?.name ? undefined : sc ? (sc.catalog ? `${sc.catalog}.${schemaLabel(sc)}` : sc.name) : o.schema,
    apply: tableApply(p, o),
    boost,
  };
}

// ------------------------------------------------------------------ entry point

/**
 * Completions at `pos` in `doc`. `explicit` = invoked with Ctrl-Space (show
 * suggestions even without typed text). Returns null when nothing fits.
 */
export async function completeSql(doc: string, pos: number, provider: MetaProvider, explicit = false): Promise<SqlCompletion | null> {
  // Read the schema list once per request (it is consulted per table option).
  const schemaList = provider.schemas();
  const p: MetaProvider = { ...provider, schemas: () => schemaList };
  const clean = blankLiterals(doc);
  // Inside a string or comment: no completion.
  if (clean[pos - 1] === " " && doc[pos - 1] !== " " && doc[pos - 1] !== "\n" && doc[pos - 1] !== "\t") return null;
  const [s0] = statementBounds(clean, pos);
  const stmtBefore = clean.slice(s0, pos);
  const [, s1] = statementBounds(clean, pos);
  const stmt = clean.slice(s0, s1);

  // Current token: dotted path ending at the cursor.
  let start = pos;
  while (start > s0 && /[\w$."`[\]]/.test(clean[start - 1])) start--;
  let token = clean.slice(start, pos);
  if (!PATH_RE.test(token)) {
    // Keep only the trailing simple word.
    const m = /[A-Za-z_][\w$]*$/.exec(token);
    start = pos - (m ? m[0].length : 0);
    token = m ? m[0] : "";
  }
  const parts = splitPath(token);
  const typedRaw = parts[parts.length - 1];
  const typed = unquote(typedRaw);
  const wordFrom = pos - typedRaw.length;
  if (!explicit && parts.length === 1 && typed.length === 0) return null;
  if (/^\d/.test(typed)) return null;

  const ctx = clauseContext(clean.slice(s0, start));
  const refs = tableRefs(stmt);
  const ctes = cteNames(stmt);
  const k = p.kind;

  // ---- dotted path: alias.col, schema.table, catalog.schema, results.x
  if (parts.length > 1) {
    const path = parts.slice(0, -1).map(unquote);
    const opts: SqlOption[] = [];
    // alias. / table. → columns
    if (path.length === 1) {
      const ref = refs.find((r) => r.alias?.toLowerCase() === path[0].toLowerCase() || (!r.alias && r.parts[r.parts.length - 1].toLowerCase() === path[0].toLowerCase()));
      if (ref) {
        const c = await columnsFor(p, ref.parts);
        for (const col of c?.columns ?? []) {
          const role = c?.roles?.[col.toLowerCase()];
          opts.push({ label: col, type: "column", detail: role ? `${c?.table} · ${role}` : c?.table, apply: quoteIdent(k, col), boost: role ? 12 : 10 });
        }
      }
    }
    // virtual schemas (DuckDB results.)
    for (const v of p.virtualTables?.() ?? []) {
      if (path.length === 1 && v.schema.toLowerCase() === path[0].toLowerCase()) opts.push({ label: v.name, type: "table", detail: v.schema, apply: quoteIdent(k, v.name) });
    }
    // catalog. → schemas
    if (threeLevel(k) && path.length === 1) {
      for (const s of p.schemas() ?? []) {
        if (s.catalog && s.catalog.toLowerCase() === path[0].toLowerCase()) {
          opts.push({ label: schemaLabel(s), type: "schema", detail: s.catalog, apply: `${quoteIdent(k, schemaLabel(s))}.`, reopen: true });
        }
      }
    }
    // schema. → tables
    const sc = findSchema(p, path);
    if (sc) {
      const objs = matchingRelations((await p.objects(sc.name, typed)) ?? [], typed, MAX_TABLE_OPTIONS);
      for (const o of objs) {
        opts.push({ label: o.name, type: o.kind === "view" || o.kind === "materialized_view" ? "view" : "table", detail: schemaLabel(sc), apply: quoteIdent(k, o.name) });
      }
    }
    // schema.table. (or catalog.schema.table.) → columns
    if (!opts.length || path.length >= 2) {
      const c = await columnsFor(p, path);
      for (const col of c?.columns ?? []) opts.push({ label: col, type: "column", detail: c?.table, apply: quoteIdent(k, col) });
    }
    const ranked = filterRank(opts, typed);
    return ranked.length ? { from: wordFrom, options: ranked } : null;
  }

  // ---- single word
  const opts: SqlOption[] = [];
  const fns = [...FUNCTIONS, ...(DIALECT_FUNCTIONS[k] ?? [])];
  if (ctx === "start" || (stmtBefore.trim() === "" && !typed)) {
    opts.push(...kw(STATEMENT_START, typed, 5));
  } else if (ctx === "table") {
    // Tables of the connection: cached first, then server search. On big
    // catalogs (10k+ tables) only names matching the typed text are turned
    // into options, best matches first.
    const def = defaultSchema(p.schemas());
    const cached = matchingRelations(p.cachedObjects(), typed, MAX_TABLE_OPTIONS);
    for (const o of cached) opts.push(tableOption(p, o, o.schema === def?.name ? 3 : 1));
    if (typed.length >= 1) {
      for (const o of await p.searchTables(typed)) opts.push(tableOption(p, o, o.schema === def?.name ? 3 : 1));
    }
    for (const c of ctes) opts.push({ label: c, type: "table", detail: "CTE", boost: 6 });
    for (const v of p.virtualTables?.() ?? []) opts.push({ label: v.name, type: "table", detail: v.schema, apply: `${v.schema}.${quoteIdent(k, v.name)}` });
    // Namespaces to drill into.
    const schemas = p.schemas() ?? [];
    const cats = new Set<string>();
    for (const s of schemas) {
      if (threeLevel(k) && s.catalog) {
        cats.add(s.catalog);
        if (s.catalog === def?.catalog) opts.push({ label: schemaLabel(s), type: "schema", apply: `${quoteIdent(k, schemaLabel(s))}.`, reopen: true });
      } else opts.push({ label: s.name, type: "schema", apply: `${quoteIdent(k, s.name)}.`, reopen: true });
    }
    for (const c of cats) opts.push({ label: c, type: "catalog", apply: `${quoteIdent(k, c)}.`, reopen: true });
    if (k === "duckdb") opts.push({ label: "results", type: "schema", detail: "outputs", apply: "results.", reopen: true });
    opts.push(...kw(["SELECT", "LATERAL"], typed, -5));
  } else if (ctx === "after_table") {
    opts.push(...kw(AFTER_TABLE, typed, 5));
  } else {
    // Expression: columns of the statement's tables, aliases, functions, keywords.
    const cols = await Promise.all(refs.map((r) => columnsFor(p, r.parts)));
    cols.forEach((c, i) => {
      for (const col of c?.columns ?? []) {
        const role = c!.roles?.[col.toLowerCase()];
        // Key columns first: filtering/joining on them is fast.
        opts.push({ label: col, type: "column", detail: role ? `${c!.table} · ${role}` : c!.table, apply: quoteIdent(k, col), boost: role ? 12 : 10 });
      }
      const r = refs[i];
      opts.push({ label: r.alias ?? r.parts[r.parts.length - 1], type: "alias", detail: r.alias ? r.parts.join(".") : "table", apply: `${quoteIdent(k, r.alias ?? r.parts[r.parts.length - 1])}.`, reopen: true, boost: 4 });
    });
    opts.push(...kw(fns, typed, 2, "function").map((f) => ({ ...f, apply: `${f.label}(` })));
    opts.push(...kw(IN_EXPRESSION, typed, 1));
  }
  const ranked = filterRank(opts, typed);
  return ranked.length ? { from: wordFrom, options: ranked } : null;
}
