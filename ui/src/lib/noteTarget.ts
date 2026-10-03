// Note & glossary targets: one or more tables (or table.columns) of the
// connection, separated by `&`, `,`, `and` or `or` (e.g. join knowledge:
// `sales.orders & crm.customers`). Helpers for the target field's completion.

export interface TargetTable {
  /** Schema id as the explorer knows it (`public`, `main.sales`). */
  schema: string;
  name: string;
  columns?: string[];
}

export interface Suggestion {
  label: string;
  /** Text that replaces the current segment. */
  insert: string;
  detail?: string;
  kind: "table" | "column";
}

const SEP = /&|,|\s(?:and|or)\s/gi;

/** [from, to) of the table path under the cursor, and its text. */
export function currentSegment(text: string, cursor: number): { from: number; to: number; word: string } {
  let from = 0;
  SEP.lastIndex = 0;
  for (let m = SEP.exec(text); m; m = SEP.exec(text)) {
    if (m.index + m[0].length > cursor) break;
    from = m.index + m[0].length;
  }
  while (from < cursor && /\s/.test(text[from])) from++;
  SEP.lastIndex = 0;
  let to = text.length;
  for (let m = SEP.exec(text); m; m = SEP.exec(text)) {
    if (m.index >= cursor) {
      to = m.index;
      break;
    }
  }
  while (to > from && /\s/.test(text[to - 1])) to--;
  return { from, to: Math.max(to, cursor), word: text.slice(from, cursor).trim() };
}

const full = (t: TargetTable) => `${t.schema}.${t.name}`;

/** The table a written path names (`orders`, `sales.orders`, `main.sales.orders`). */
export function findTable(tables: TargetTable[], path: string): TargetTable | undefined {
  const p = path.toLowerCase();
  return (
    tables.find((t) => full(t).toLowerCase() === p) ??
    tables.find((t) => full(t).toLowerCase().endsWith(`.${p}`)) ??
    tables.find((t) => t.name.toLowerCase() === p)
  );
}

/**
 * Suggestions for the segment `word`: tables whose name or `schema.table`
 * matches (prefix first), or, after `table.`, that table's columns.
 */
export function suggest(word: string, tables: TargetTable[], limit = 30): Suggestion[] {
  const w = word.toLowerCase();
  const dot = w.lastIndexOf(".");
  const out: Suggestion[] = [];
  if (dot > 0) {
    const t = findTable(tables, word.slice(0, dot));
    const colPart = w.slice(dot + 1);
    for (const c of t?.columns ?? []) {
      if (c.toLowerCase().startsWith(colPart)) out.push({ label: c, insert: `${full(t!)}.${c}`, detail: full(t!), kind: "column" });
      if (out.length >= limit) return out;
    }
  }
  const scored: [number, TargetTable][] = [];
  const seen = new Set<string>();
  for (const t of tables) {
    const f = full(t).toLowerCase();
    if (seen.has(f)) continue;
    seen.add(f);
    const n = t.name.toLowerCase();
    const r = !w ? 2 : n.startsWith(w) || f.startsWith(w) ? 0 : n.includes(w) || f.includes(w) ? 1 : -1;
    if (r >= 0) scored.push([r, t]);
  }
  scored.sort((a, b) => a[0] - b[0] || a[1].name.length - b[1].name.length || full(a[1]).localeCompare(full(b[1])));
  for (const [, t] of scored) {
    if (out.length >= limit) break;
    out.push({ label: t.name, insert: full(t), detail: t.schema, kind: "table" });
  }
  return out;
}

/** Replace the segment [from, to) with `insert`. Returns the text and the new cursor. */
export function applySuggestion(text: string, seg: { from: number; to: number }, insert: string): { text: string; cursor: number } {
  const next = text.slice(0, seg.from) + insert + text.slice(seg.to);
  return { text: next, cursor: seg.from + insert.length };
}
