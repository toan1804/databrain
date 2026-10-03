// Query hints from table layout (indexes, partitions, cluster keys): which
// filters can skip data and which ones defeat it. Pure logic over SQL text;
// components/queryHintsExt.ts puts the results in the editor.
import { blankLiterals, splitPath, unquote } from "./sqlComplete";
import type { ConnectorKind, TableLayout } from "./types";

export type HintLevel = "warn" | "info";

export interface Hint {
  /** Offsets in the analysed text. */
  from: number;
  to: number;
  level: HintLevel;
  message: string;
}

/** A table reference with its position (`FROM x a`, `JOIN y`, `UPDATE z`). */
export interface RefAt {
  parts: string[];
  alias?: string;
  /** Position of the table name (not the alias). */
  from: number;
  to: number;
}

export interface LayoutRef {
  ref: RefAt;
  layout: TableLayout;
}

const IDENT = String.raw`(?:[A-Za-z_][\w$]*|"(?:[^"]|"")*"|\x60[^\x60]*\x60|\[[^\]]*\])`;
const PATH = String.raw`(?:${IDENT}\s*\.\s*)*${IDENT}`;

const NOT_ALIAS = new Set([
  "where", "join", "left", "right", "inner", "outer", "full", "cross", "on", "group", "order", "limit", "union", "having", "using", "set", "values",
  "natural", "lateral", "window", "qualify", "as", "select", "from", "returning", "pivot", "unpivot", "tablesample", "with", "into", "except",
  "intersect", "fetch", "offset", "for", "when", "then",
]);

const KEYWORDS = new Set([
  "and", "or", "not", "in", "is", "null", "like", "ilike", "between", "exists", "case", "when", "then", "else", "end", "true", "false", "as",
  "select", "from", "where", "on", "join", "any", "all", "some", "interval", "date", "timestamp", "time", "distinct", "escape", "similar",
  "to", "cast", "over", "partition", "by", "asc", "desc", "nulls", "first", "last", "current_date", "current_timestamp", "now",
]);

/** Readable number: 1234567 → 1.2M. */
export function compact(n: number): string {
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e4) return `${Math.round(n / 1e3)}K`;
  return String(n);
}

/** Tables a statement reads or changes, with positions (INSERT targets excluded). */
export function refsWithPos(stmt: string): RefAt[] {
  const clean = blankLiterals(stmt);
  const out: RefAt[] = [];
  const re = new RegExp(String.raw`\b(from|join|update)\s+(${PATH})(?:\s+(?:as\s+)?(${IDENT}))?`, "gi");
  const push = (path: string, at: number, alias?: string) => {
    const parts = splitPath(path.replace(/\s*\.\s*/g, ".").trim()).map(unquote);
    if (!parts.length || !parts[parts.length - 1]) return;
    if (/^(select|lateral|unnest)$/i.test(parts[0])) return;
    const a = alias && !NOT_ALIAS.has(alias.toLowerCase()) ? unquote(alias) : undefined;
    out.push({ parts, alias: a, from: at, to: at + path.length });
  };
  let m: RegExpExecArray | null;
  while ((m = re.exec(clean))) {
    // FROM read_parquet(...) / FROM (subquery): not a table.
    if (clean.slice(m.index + m[0].length).trimStart().startsWith("(") && !m[3]) continue;
    const at = m.index + m[0].indexOf(m[2], m[1].length);
    push(m[2], at, m[3]);
    if (m[1].toLowerCase() !== "from") continue;
    // FROM a x, b y
    const more = new RegExp(String.raw`^\s*,\s*(${PATH})(?:\s+(?:as\s+)?(${IDENT}))?`, "i");
    let pos = m.index + m[0].length;
    let mm: RegExpExecArray | null;
    while ((mm = more.exec(clean.slice(pos)))) {
      push(mm[1], pos + mm[0].indexOf(mm[1]), mm[2]);
      pos += mm[0].length;
    }
  }
  return out;
}

/** [from, to) ranges of WHERE / ON / HAVING / QUALIFY predicates (any nesting level). */
export function predicateRanges(clean: string): [number, number][] {
  const out: [number, number][] = [];
  const re = /\b(where|on|having|qualify)\b/gi;
  const stop = /^(group|order|limit|having|qualify|window|union|intersect|except|returning|join|left|right|inner|full|cross|natural|where|on|fetch|offset|select|from|using)\b/i;
  let m: RegExpExecArray | null;
  while ((m = re.exec(clean))) {
    const start = m.index + m[0].length;
    let depth = 0;
    let i = start;
    for (; i < clean.length; i++) {
      const c = clean[i];
      if (c === "(") depth++;
      else if (c === ")") {
        if (depth === 0) break;
        depth--;
      } else if (c === ";") break;
      else if (depth === 0 && /\s/.test(clean[i - 1] ?? " ") && /[a-z]/i.test(c) && stop.test(clean.slice(i, i + 12))) break;
    }
    out.push([start, i]);
  }
  return out;
}

interface ColUse {
  /** Qualifier as written (`o` in `o.day`), lowercase. */
  qual?: string;
  name: string;
  from: number;
  to: number;
  /** Function or cast wrapping the column (`date`, `lower`, `::`). */
  wrappedBy?: string;
  /** Text of the wrapping call, e.g. `lower(email)`. */
  wrapText?: string;
  /** `LIKE '%…'` (leading wildcard). */
  leadingWildcard?: boolean;
}

/** Column references inside predicates. */
export function columnUses(stmt: string): ColUse[] {
  const clean = blankLiterals(stmt);
  const out: ColUse[] = [];
  for (const [a, b] of predicateRanges(clean)) {
    const seg = clean.slice(a, b);
    const re = new RegExp(String.raw`(?:(${IDENT})\s*\.\s*)?(${IDENT})`, "g");
    let m: RegExpExecArray | null;
    while ((m = re.exec(seg))) {
      const raw = m[2];
      const name = unquote(raw);
      const prevCh = seg[m.index - 1];
      if (prevCh === "." || /^\d/.test(raw)) continue;
      // Type names: x::text, cast(x as varchar).
      if (/(::|\bas)\s*$/i.test(seg.slice(0, m.index))) continue;
      const after = seg.slice(m.index + m[0].length);
      if (/^\s*\(/.test(after)) continue; // function name
      if (!m[1] && KEYWORDS.has(name.toLowerCase())) continue;
      if (/^\s*\./.test(after)) continue; // qualifier of a longer path
      const from = a + m.index + m[0].length - raw.length;
      const use: ColUse = { qual: m[1] ? unquote(m[1]).toLowerCase() : undefined, name, from, to: from + raw.length };
      if (/^\s*::/.test(after)) {
        use.wrappedBy = "::";
        use.wrapText = (m[0] + after.match(/^\s*::\s*\w+/)![0]).trim();
      } else {
        // Innermost unclosed "(" before the column, inside this predicate.
        let depth = 0;
        for (let j = m.index - 1; j >= 0; j--) {
          const c = seg[j];
          if (c === ")") depth++;
          else if (c === "(") {
            if (depth > 0) {
              depth--;
              continue;
            }
            const fn = /([A-Za-z_][\w$]*)\s*$/.exec(seg.slice(0, j));
            const f = fn?.[1].toLowerCase();
            const isCall = !!f && (!KEYWORDS.has(f) || f === "cast" || f === "date");
            if (isCall) {
              // Closing paren of this call.
              let d = 0;
              let k = j;
              for (; k < seg.length; k++) {
                if (seg[k] === "(") d++;
                else if (seg[k] === ")" && --d === 0) break;
              }
              use.wrappedBy = f;
              use.wrapText = stmt.slice(a + (fn!.index ?? 0), a + k + 1).trim();
            }
            break;
          } else if (/[=<>]/.test(c) || /\b(and|or)\s*$/i.test(seg.slice(Math.max(0, j - 4), j + 1))) break;
        }
      }
      // Literals are blanked in `clean`; read the pattern from the original text.
      if (/^\s*(?:not\s+)?i?like\s+'[%_]/i.test(stmt.slice(from + raw.length))) use.leadingWildcard = true;
      if (!out.some((o) => o.from === use.from)) out.push(use);
    }
  }
  return out;
}

const norm = (s: string) => s.toLowerCase().replace(/["`[\]\s]/g, "");

/** Short role of a column for completion badges (`partition key`, `PK`, `index`). */
export function columnRoles(l: TableLayout): Record<string, string> {
  const r: Record<string, string> = {};
  const set = (c: string, role: string) => {
    const k = c.toLowerCase();
    if (!r[k]) r[k] = role;
  };
  for (const c of l.partition_by) set(c, "partition key");
  l.cluster_by.forEach((c, i) => set(c, i === 0 ? "cluster key" : `cluster key ${i + 1}`));
  for (const i of l.indexes) if (i.columns[0]) set(i.columns[0], i.primary ? "primary key" : i.unique ? "unique index" : "indexed");
  return r;
}

/** Lines describing a table's layout (hover card). */
export function layoutSummary(l: TableLayout): string[] {
  const out: string[] = [];
  if (l.partition_by.length) {
    const kind = l.partition_kind ? ` (${l.partition_kind})` : "";
    out.push(`Partitioned by ${l.partition_by.join(", ")}${kind}${l.requires_partition_filter ? " · partition filter required" : ""}`);
  }
  if (l.cluster_by.length) out.push(`Clustered by ${l.cluster_by.join(", ")}`);
  for (const i of l.indexes.slice(0, 8)) {
    const what = i.primary ? "Primary key" : i.unique ? `Unique index ${i.name}` : `Index ${i.name}`;
    out.push(`${what} (${i.columns.join(", ")})${i.method ? ` · ${i.method}` : ""}`);
  }
  if (l.indexes.length > 8) out.push(`…and ${l.indexes.length - 8} more indexes`);
  if (l.row_estimate != null && l.row_estimate > 0) out.push(`~${compact(l.row_estimate)} rows`);
  out.push(...l.notes);
  return out;
}

/** Tables where a missing index on a filter is worth mentioning. */
export const LARGE_TABLE_ROWS = 100_000;

/**
 * Hints for one statement. `tables` are the statement's table references
 * with their layout (unknown tables are left out).
 */
export function analyzeStatement(stmt: string, tables: LayoutRef[], kind?: ConnectorKind): Hint[] {
  if (!tables.length) return [];
  const uses = columnUses(stmt);
  const hints: Hint[] = [];
  const name = (t: LayoutRef) => t.ref.parts[t.ref.parts.length - 1];
  const single = tables.length === 1;
  // Uses that belong to a table: qualified by its alias/name, or bare.
  const usesOf = (t: LayoutRef, keys: string[]) => {
    const quals = new Set([t.ref.alias?.toLowerCase(), name(t).toLowerCase()].filter(Boolean) as string[]);
    const lk = new Set(keys.map((k) => k.toLowerCase()));
    return uses.filter((u) => lk.has(u.name.toLowerCase()) && (u.qual ? quals.has(u.qual) : true));
  };
  const allUsesOf = (t: LayoutRef) => {
    const quals = new Set([t.ref.alias?.toLowerCase(), name(t).toLowerCase()].filter(Boolean) as string[]);
    return uses.filter((u) => (u.qual ? quals.has(u.qual) : single));
  };

  for (const t of tables) {
    const l = t.layout;
    const tn = name(t);
    // ---- partitions
    if (l.partition_by.length) {
      const pu = usesOf(t, l.partition_by);
      const keys = l.partition_by.join(", ");
      if (!pu.length) {
        hints.push({
          from: t.ref.from,
          to: t.ref.to,
          level: "warn",
          message: l.requires_partition_filter
            ? `${tn} requires a filter on its partition column ${keys}; the query is rejected without one.`
            : `${tn} is partitioned by ${keys}. Filter on ${l.partition_by.length > 1 ? "them" : "it"} to read only the partitions you need${kind === "bigquery" ? " (LIMIT does not reduce bytes scanned)" : ""}.`,
        });
      }
      for (const u of pu.filter((u) => u.wrappedBy)) {
        hints.push({
          from: u.from,
          to: u.to,
          level: "warn",
          message: `${u.wrapText ?? u.wrappedBy}: wrapping the partition column ${u.name} in ${u.wrappedBy === "::" ? "a cast" : `${u.wrappedBy}()`} can prevent partition pruning. Compare the bare column with a range instead, e.g. ${u.name} >= '2024-01-01' AND ${u.name} < '2024-02-01'.`,
        });
      }
    }
    // ---- clustering
    if (l.cluster_by.length) {
      const cu = usesOf(t, l.cluster_by);
      if (!cu.length && !(l.partition_by.length && !usesOf(t, l.partition_by).length)) {
        hints.push({
          from: t.ref.from,
          to: t.ref.to,
          level: "info",
          message: `${tn} is clustered by ${l.cluster_by.join(", ")}. Filters on ${l.cluster_by[0]} let the engine skip most of the data.`,
        });
      }
      for (const u of cu.filter((u) => u.wrappedBy && norm(u.wrapText ?? "") !== norm(l.cluster_by[0]))) {
        hints.push({ from: u.from, to: u.to, level: "info", message: `${u.wrapText}: a function on the cluster key ${u.name} can stop the engine from skipping data.` });
      }
    }
    // ---- indexes
    if (l.indexes.length) {
      const exprIdx = new Set(l.indexes.flatMap((i) => i.columns.map(norm)));
      const lead = new Set(l.indexes.map((i) => i.columns[0]?.toLowerCase()).filter(Boolean));
      const iu = uses.filter((u) => lead.has(u.name.toLowerCase()) && usesOf(t, [u.name]).includes(u));
      for (const u of iu) {
        const idx = l.indexes.find((i) => i.columns[0]?.toLowerCase() === u.name.toLowerCase())!;
        const label = idx.primary ? "the primary key" : `index ${idx.name}`;
        if (u.wrappedBy && !exprIdx.has(norm(u.wrapText ?? ""))) {
          hints.push({ from: u.from, to: u.to, level: "warn", message: `${u.wrapText}: ${label} on ${u.name} cannot be used when the column is wrapped in ${u.wrappedBy === "::" ? "a cast" : `${u.wrappedBy}()`}. Compare the bare column, or add an expression index.` });
        } else if (u.leadingWildcard) {
          hints.push({ from: u.from, to: u.to, level: "info", message: `LIKE with a leading wildcard cannot use ${label} on ${u.name}.` });
        }
      }
      // Filters on a big table with no index starting with any filtered column.
      const filtered = allUsesOf(t);
      const big = (l.row_estimate ?? 0) >= LARGE_TABLE_ROWS;
      const leadExpr = new Set(l.indexes.map((i) => norm(i.columns[0] ?? "")));
      const indexable = (u: ColUse) => (lead.has(u.name.toLowerCase()) && !u.wrappedBy) || (!!u.wrapText && leadExpr.has(norm(u.wrapText)));
      const flagged = hints.some((h) => filtered.some((u) => u.from === h.from));
      if (big && filtered.length && !flagged && !filtered.some(indexable) && !l.partition_by.length) {
        const u = filtered[0];
        const list = l.indexes.slice(0, 4).map((i) => `${i.primary ? "PK" : i.name} (${i.columns.join(", ")})`).join("; ");
        hints.push({
          from: u.from,
          to: u.to,
          level: "info",
          message: `No index on ${tn} (~${compact(l.row_estimate!)} rows) starts with ${[...new Set(filtered.map((f) => f.name))].join(", ")}: this filter scans the table. Indexes: ${list}.`,
        });
      }
    }
  }
  // Same range reported twice (e.g. alias reused): keep the first.
  const seen = new Set<string>();
  return hints.filter((h) => {
    const k = `${h.from}|${h.to}|${h.message}`;
    if (seen.has(k)) return false;
    seen.add(k);
    return true;
  });
}
