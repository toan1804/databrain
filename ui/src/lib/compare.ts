import type { CompareKind, CompareOptions, ViewSpec } from "./types";

// Column matching for "Compare outputs": equal names, then names equal
// ignoring case (each column once), plus pairs the user adds. A pair the
// user adds never replaces another: debit = debit, credit = credit and
// debit = credit can all be compared.

export interface ColumnPair {
  before: string;
  after: string;
  /** The pair's column in the result (as the backend's `pair_labels`). */
  label: string;
  how: "manual" | "same" | "case";
}

/** What the user changed: pairs added, after columns not matched automatically. */
export interface PairEdits {
  manual: [string, string][];
  excluded: string[];
}

/**
 * Each pair's result column: the after name, or `before → after` when that
 * after column is in another pair too (the one with the same name, else
 * the first, keeps the plain name). Same rule as the backend.
 */
export function pairLabels(pairs: [string, string][]): string[] {
  const keeper = (a: string) => {
    const same = pairs.findIndex(([b, x]) => x === a && b === a);
    if (same >= 0) return same;
    const cased = pairs.findIndex(([b, x]) => x === a && b.toLowerCase() === a.toLowerCase());
    return cased >= 0 ? cased : pairs.findIndex(([, x]) => x === a);
  };
  return pairs.map(([b, a], i) => (keeper(a) === i ? a : `${b} → ${a}`));
}

export function pairColumns(before: string[], after: string[], manual: [string, string][], excluded: string[] = []): { pairs: ColumnPair[]; onlyBefore: string[]; onlyAfter: string[] } {
  const usedB = new Set<string>();
  const usedA = new Set<string>(excluded);
  const found: Omit<ColumnPair, "label">[] = [];
  for (const exact of [true, false]) {
    for (const a of after) {
      if (usedA.has(a)) continue;
      const b = before.find((x) => !usedB.has(x) && (exact ? x === a : x.toLowerCase() === a.toLowerCase()));
      if (b === undefined) continue;
      usedB.add(b);
      usedA.add(a);
      found.push({ before: b, after: a, how: exact ? "same" : "case" });
    }
  }
  for (const [b, a] of manual) {
    if (!before.includes(b) || !after.includes(a) || found.some((p) => p.before === b && p.after === a)) continue;
    found.push({ before: b, after: a, how: "manual" });
  }
  // The after output's column order; pairs on one column in the order they came.
  const sorted = found.map((p, i) => ({ p, i })).sort((x, y) => after.indexOf(x.p.after) - after.indexOf(y.p.after) || x.i - y.i).map((x) => x.p);
  const labels = pairLabels(sorted.map((p) => [p.before, p.after]));
  const pairs = sorted.map((p, i) => ({ ...p, label: labels[i] }));
  const pairedA = new Set(pairs.map((p) => p.after));
  const pairedB = new Set(pairs.map((p) => p.before));
  return { pairs, onlyBefore: before.filter((b) => !pairedB.has(b)), onlyAfter: after.filter((a) => !pairedA.has(a)) };
}

/** Add `b = a` to the compared pairs (the pairs already there stay). */
export function addPair(st: PairEdits, b: string, a: string): PairEdits {
  if (st.manual.some(([x, y]) => x === b && y === a)) return st;
  return { ...st, manual: [...st.manual, [b, a]] };
}

/** Stop comparing a pair: one the user added goes; a matched one is left out. */
export function removePair(st: PairEdits, p: Pick<ColumnPair, "before" | "after" | "how">): PairEdits {
  if (p.how === "manual") return { ...st, manual: st.manual.filter(([x, y]) => !(x === p.before && y === p.after)) };
  return { ...st, excluded: st.excluded.includes(p.after) ? st.excluded : [...st.excluded, p.after] };
}

/** Compare `p.after` with another before column instead (`b`). */
export function changePair(st: PairEdits, p: Pick<ColumnPair, "before" | "after" | "how">, b: string): PairEdits {
  return b === p.before ? st : addPair(removePair(st, p), b, p.after);
}

/**
 * Pairs typed as `a = a1, b = b2` (also `->`, `→`, `↔`, `:`; commas,
 * semicolons or new lines between pairs). `r1.a = r2.a1` works too: a
 * prefix before the first dot is dropped when the whole name isn't a column.
 */
export function parseMapping(text: string, before: string[], after: string[]): { pairs: [string, string][]; errors: string[] } {
  const pick = (name: string, cols: string[]): string | undefined => {
    const t = name.trim().replace(/^["`[]|["`\]]$/g, "");
    const find = (n: string) => cols.find((c) => c === n) ?? cols.find((c) => c.toLowerCase() === n.toLowerCase());
    return find(t) ?? (t.includes(".") ? find(t.slice(t.indexOf(".") + 1)) : undefined);
  };
  const pairs: [string, string][] = [];
  const errors: string[] = [];
  for (const part of text.split(/[,;\n]+/).map((p) => p.trim()).filter(Boolean)) {
    const m = part.split(/\s*(?:<->|->|=>|↔|→|=|:)\s*/);
    if (m.length !== 2 || !m[0] || !m[1]) {
      errors.push(`"${part}": write it as before = after`);
      continue;
    }
    const b = pick(m[0], before);
    const a = pick(m[1], after);
    if (!b) errors.push(`${m[0]} is not a column of the before output`);
    else if (!a) errors.push(`${m[1]} is not a column of the after output`);
    else pairs.push([b, a]);
  }
  return { pairs, errors };
}

// ------------------------------------------------- how values are compared


export const KIND_LABEL: Record<CompareKind, string> = {
  date_time: "date/time",
  date: "date only",
  number: "number",
  text: "text",
  bool: "boolean",
  exact: "exact",
};

/** What each kind means, for the "Compare as" list. */
export const KIND_HELP: Record<CompareKind, string> = {
  date_time: "2026-11-11 00:00:00 = 2026-11-11",
  date: "Ignore the time of day",
  number: "10.50 = 10.5 = '10.5'",
  text: "As text (spaces at the ends ignored unless you turn that off)",
  bool: "true = 1 = Y = yes",
  exact: "As the database gave it, as text",
};

/** Arrow type names as people read them: `Timestamp(µs)` → `timestamp`, `Utf8` → `text`. */
export function shortType(t: string): string {
  const s = t.trim();
  const dec = /^Decimal(?:128|256)\((\d+),\s*(-?\d+)\)$/.exec(s);
  if (dec) return `decimal(${dec[1]},${dec[2]})`;
  if (/^(Large)?Utf8(View)?$/.test(s)) return "text";
  // `Timestamp(µs, "UTC")` or `Timestamp(Microsecond, Some("UTC"))` has a time zone.
  if (/^Timestamp/.test(s)) return /,\s*(Some|")/.test(s) ? "timestamptz" : "timestamp";
  if (/^Date(32|64)$/.test(s)) return "date";
  if (/^Time(32|64)/.test(s)) return "time";
  if (/^U?Int(8|16|32|64)$/.test(s)) return s.toLowerCase();
  if (/^Float(16|32|64)$/.test(s)) return s === "Float32" ? "float" : "double";
  if (s === "Boolean") return "boolean";
  if (/Binary/.test(s)) return "binary";
  return s.toLowerCase();
}

/** The options as the dialog edits them. */
export interface CompareForm {
  trim: boolean;
  ignoreCase: boolean;
  emptyIsNull: boolean;
  /** As typed; empty = exact. */
  tolerance: string;
  /** After column → kind chosen by the user. */
  kinds: Record<string, CompareKind>;
}

export const DEFAULT_COMPARE_FORM: CompareForm = { trim: true, ignoreCase: false, emptyIsNull: false, tolerance: "", kinds: {} };

/** The form for saved options (a compare tab being edited). */
export function formFromOptions(o: CompareOptions): CompareForm {
  return {
    trim: o.trim,
    ignoreCase: o.ignore_case,
    emptyIsNull: o.empty_is_null,
    tolerance: o.tolerance == null ? "" : String(o.tolerance),
    kinds: Object.fromEntries(o.kinds),
  };
}

/** The tolerance typed: `null` when empty, `NaN` when not a positive number. */
export function parseTolerance(text: string): number | null {
  const t = text.trim().replace(",", ".");
  if (!t) return null;
  const n = Number(t);
  return Number.isFinite(n) && n > 0 ? n : NaN;
}

/** Backend options from the form (`kinds` only for compared columns). */
export function compareOptions(f: CompareForm, compared: string[]): CompareOptions {
  const tol = parseTolerance(f.tolerance);
  return {
    trim: f.trim,
    ignore_case: f.ignoreCase,
    empty_is_null: f.emptyIsNull,
    tolerance: tol === null || Number.isNaN(tol) ? null : tol,
    kinds: Object.entries(f.kinds).filter(([c]) => compared.includes(c)),
  };
}

// ------------------------------------------------------ the compare view

/** The columns named in a changed row's `_changed` cell (`amt, code`), out of `columns`. */
export function changedIn(cell: string | null | undefined, columns: string[]): Set<string> {
  const out = new Set<string>();
  if (!cell) return out;
  const padded = `, ${cell}, `;
  for (const c of columns) if (padded.includes(`, ${c}, `)) out.add(c);
  return out;
}

/** Where things are in a diff result's columns. */
export interface DiffLayout {
  change: number;
  /** `_changed` (changed rows only), or -1. */
  changed: number;
  /** After-side value of each column, and its `(before)` value in changed rows (or -1). */
  value: Record<string, number>;
  before: Record<string, number>;
}

export function diffLayout(resultColumns: string[], compared: string[]): DiffLayout {
  const at = (n: string) => resultColumns.indexOf(n);
  const value: Record<string, number> = {};
  const before: Record<string, number> = {};
  for (const c of compared) {
    value[c] = at(c);
    before[c] = at(`${c} (before)`);
  }
  return { change: at("_change"), changed: at("_changed"), value, before };
}

/** Which rows of a compare to list. */
export type DiffFilter = "changed" | "added" | "removed" | "all";

/** The backend view for a filter and, optionally, one changed column. */
export function diffView(layout: DiffLayout, filter: DiffFilter, column: string | null): ViewSpec {
  const filters: ViewSpec["filters"] = [];
  if (filter !== "all" && layout.change >= 0) filters.push({ column: layout.change, op: "equals", value: filter });
  // Over-includes only when a name is part of another's ("amt" in "amt_total"); the cells say exactly.
  if (column && layout.changed >= 0) filters.push({ column: layout.changed, op: "contains", value: column });
  return { filters, sort: [] };
}
