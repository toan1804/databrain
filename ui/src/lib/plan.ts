// Reading query plans: flattening for display, time spent in each step,
// and the warnings worth pointing at (full scans, bad row estimates).

import type { PlanNode } from "./types";

export interface PlanRow {
  node: PlanNode;
  depth: number;
  /** Path of child indexes ("0.1.2"): stable id within a plan. */
  id: string;
  /** Time spent in this step alone (ms, all loops), when analyzed. */
  selfMs?: number;
  /** Total time of the step and its children (ms, all loops). */
  totalMs?: number;
  warnings: string[];
}

/** Time of a node and its subtree over all loops. */
export function totalMs(n: PlanNode): number | undefined {
  if (n.time_ms === undefined) return undefined;
  return n.time_ms * Math.max(1, n.loops ?? 1);
}

/** Time in the step itself: its total minus its children's. */
export function selfMs(n: PlanNode): number | undefined {
  const t = totalMs(n);
  if (t === undefined) return undefined;
  const kids = n.children.reduce((s, c) => s + (totalMs(c) ?? 0), 0);
  return Math.max(0, t - kids);
}

const FULL_SCAN = /\b(seq scan|table scan|full scan|tablescan|seq_scan|table access full|index full scan|clustered index scan)\b|^scan [a-z_"`]/i;

/** Does the step read a whole table (or index), whatever it keeps of it? */
export function isFullScan(n: PlanNode): boolean {
  return FULL_SCAN.test(n.op);
}

/**
 * Rows a step went through, as far as the plan tells: actual rows (× loops)
 * plus the ones its filter dropped when analyzed (PostgreSQL), else the
 * estimate. A scan's estimate is the rows it keeps after its WHERE on most
 * engines, not the rows it reads, so it under-counts for a filtered scan.
 */
function rowsOf(n: PlanNode): number | undefined {
  if (n.actual_rows !== undefined) {
    const removed = Number(n.extra.find(([k]) => k === "Rows Removed by Filter")?.[1]);
    return (n.actual_rows + (Number.isFinite(removed) ? removed : 0)) * Math.max(1, n.loops ?? 1);
  }
  return n.rows;
}

export function warningsOf(n: PlanNode): string[] {
  const out: string[] = [];
  const rows = rowsOf(n);
  if (isFullScan(n) && (rows === undefined || rows >= 1000)) out.push(`Full scan${rows !== undefined ? ` of ${fmtNum(rows)} rows` : ""}`);
  if (n.rows !== undefined && n.actual_rows !== undefined) {
    const est = Math.max(1, n.rows);
    const act = Math.max(1, n.actual_rows);
    const ratio = act / est;
    if (ratio >= 10 || ratio <= 0.1) out.push(`Estimated ${fmtNum(n.rows)} rows, got ${fmtNum(n.actual_rows)} (${ratio >= 10 ? `${Math.round(ratio)}× more` : `${Math.round(1 / ratio)}× fewer`})`);
  }
  if (n.loops === 0) out.push("Never executed");
  return out;
}

export function flattenPlan(roots: PlanNode[]): PlanRow[] {
  const out: PlanRow[] = [];
  const walk = (n: PlanNode, depth: number, id: string) => {
    out.push({ node: n, depth, id, selfMs: selfMs(n), totalMs: totalMs(n), warnings: warningsOf(n) });
    n.children.forEach((c, i) => walk(c, depth + 1, `${id}.${i}`));
  };
  roots.forEach((r, i) => walk(r, 0, String(i)));
  return out;
}

/** The step that took the most time on its own (analyzed plans). */
export function hottest(rows: PlanRow[]): PlanRow | undefined {
  let best: PlanRow | undefined;
  for (const r of rows) if (r.selfMs !== undefined && (!best || r.selfMs > (best.selfMs ?? 0))) best = r;
  return best && (best.selfMs ?? 0) > 0 ? best : undefined;
}

export function fmtNum(n: number): string {
  if (Math.abs(n) >= 1e6) return `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1)}M`;
  if (Math.abs(n) >= 1e4) return `${(n / 1e3).toFixed(n >= 1e5 ? 0 : 1)}k`;
  return Number.isInteger(n) ? String(n) : n.toFixed(2);
}

export function fmtMs(ms: number): string {
  if (ms >= 1000) return `${(ms / 1000).toFixed(2)} s`;
  if (ms >= 10) return `${ms.toFixed(0)} ms`;
  return `${ms.toFixed(2)} ms`;
}

// ------------------------------------------------------------- comparing

/** Numbers that sum up a plan, for before/after. */
export interface PlanMetrics {
  /** Total estimated cost (the roots' costs, in the engine's units). */
  cost?: number;
  /** Estimated rows of the result (first root). */
  rows?: number;
  /** Measured time (ANALYZE): execution, else the roots' time. */
  timeMs?: number;
  steps: number;
  /** Full scans, as "op object" (one per step). */
  fullScans: string[];
  warnings: number;
}

export function planMetrics(roots: PlanNode[], executionMs?: number): PlanMetrics {
  const rows = flattenPlan(roots);
  const costs = roots.map((r) => r.cost).filter((c): c is number => c !== undefined);
  const times = roots.map(totalMs).filter((t): t is number => t !== undefined);
  return {
    cost: costs.length ? costs.reduce((a, b) => a + b, 0) : undefined,
    rows: roots[0]?.rows,
    timeMs: executionMs ?? (times.length ? times.reduce((a, b) => a + b, 0) : undefined),
    steps: rows.length,
    fullScans: rows.filter((r) => r.warnings.some((w) => w.startsWith("Full scan"))).map((r) => `${r.node.op}${r.node.object ? ` ${r.node.object}` : ""}`),
    warnings: rows.reduce((n, r) => n + r.warnings.length, 0),
  };
}

/** A before → after change: the share it moved, and whether lower is better here. */
export interface Delta {
  before?: number;
  after?: number;
  /** after / before − 1 (undefined when either is missing or before is 0). */
  ratio?: number;
  /** true = improved, false = got worse, null = same or not comparable. */
  better: boolean | null;
}

export function delta(before: number | undefined, after: number | undefined, lowerIsBetter = true): Delta {
  if (before === undefined || after === undefined) return { before, after, better: null };
  const ratio = before !== 0 ? after / before - 1 : undefined;
  // Changes under 1% are noise in estimates.
  const same = before === after || (ratio !== undefined && Math.abs(ratio) < 0.01);
  return { before, after, ratio, better: same ? null : lowerIsBetter ? after < before : after > before };
}

/** "−42%", "+3.1×", "same". */
export function fmtDelta(d: Delta): string {
  if (d.better === null && (d.ratio === undefined || Math.abs(d.ratio) < 0.01)) return d.before === undefined || d.after === undefined ? "" : "same";
  if (d.ratio === undefined) return d.after! > d.before! ? "up from 0" : "";
  if (d.ratio >= 1) return `${(d.ratio + 1).toFixed(1)}×`;
  return `${d.ratio > 0 ? "+" : "−"}${Math.round(Math.abs(d.ratio) * 100)}%`;
}

/** What a step does and on what: steps with the same key are "the same step". */
const stepKey = (n: PlanNode) => `${n.op}|${n.object ?? ""}`;

export interface PlanComparison {
  before: PlanMetrics;
  after: PlanMetrics;
  cost: Delta;
  time: Delta;
  steps: Delta;
  scans: Delta;
  /** Full scans the rewrite removed, and the ones it added. */
  scansRemoved: string[];
  scansAdded: string[];
  /** Row ids (flattenPlan) only in one plan: steps the rewrite dropped or added. */
  gone: Set<string>;
  added: Set<string>;
}

/**
 * The table a full scan reads: its object without the alias ("on public.orders o"
 * → "public.orders"), else the step itself (SQLite's "SCAN orders").
 */
function scannedRelation(n: PlanNode): string {
  const obj = n.object?.replace(/^on\s+/i, "").trim();
  return (obj ? obj.split(/\s+/)[0] : n.op).toLowerCase();
}

const scanLabel = (n: PlanNode) => `${n.op}${n.object ? ` ${n.object}` : ""}`;

/**
 * Full scans of each plan, counted alike on both sides. A scan of few rows
 * is not flagged on its own, but the row count of a scan is mostly the
 * estimate after its WHERE, so a rewrite that only adds a selective filter
 * would seem to drop the scan while the table is still read in full. So a
 * full scan of a table the other plan flags is counted too: it is still there.
 */
function comparableScans(rows: PlanRow[], other: PlanRow[]): PlanRow[] {
  const flagged = (r: PlanRow) => r.warnings.some((w) => w.startsWith("Full scan"));
  const otherFlagged = new Set(other.filter(flagged).map((r) => scannedRelation(r.node)));
  return rows.filter((r) => flagged(r) || (isFullScan(r.node) && otherFlagged.has(scannedRelation(r.node))));
}

/** Compare two plans of the same engine (costs are only comparable within one engine). */
export function comparePlans(before: { roots: PlanNode[]; execution_ms?: number }, after: { roots: PlanNode[]; execution_ms?: number }): PlanComparison {
  const br = flattenPlan(before.roots);
  const ar = flattenPlan(after.roots);
  const bScans = comparableScans(br, ar);
  const aScans = comparableScans(ar, br);
  const b = { ...planMetrics(before.roots, before.execution_ms), fullScans: bScans.map((r) => scanLabel(r.node)) };
  const a = { ...planMetrics(after.roots, after.execution_ms), fullScans: aScans.map((r) => scanLabel(r.node)) };
  // Scans in `xs` with no scan of the same table left in `ys` (one for one).
  const minus = (xs: PlanRow[], ys: PlanRow[]) => {
    const left = ys.map((r) => scannedRelation(r.node));
    return xs
      .filter((x) => {
        const i = left.indexOf(scannedRelation(x.node));
        if (i < 0) return true;
        left.splice(i, 1);
        return false;
      })
      .map((r) => scanLabel(r.node));
  };
  const onlyIn = (rows: PlanRow[], other: PlanRow[]) => {
    const counts = new Map<string, number>();
    for (const r of other) counts.set(stepKey(r.node), (counts.get(stepKey(r.node)) ?? 0) + 1);
    const out = new Set<string>();
    for (const r of rows) {
      const k = stepKey(r.node);
      const n = counts.get(k) ?? 0;
      if (n > 0) counts.set(k, n - 1);
      else out.add(r.id);
    }
    return out;
  };
  return {
    before: b,
    after: a,
    cost: delta(b.cost, a.cost),
    time: delta(b.timeMs, a.timeMs),
    steps: delta(b.steps, a.steps),
    scans: delta(b.fullScans.length, a.fullScans.length),
    scansRemoved: minus(bScans, aScans),
    scansAdded: minus(aScans, bScans),
    gone: onlyIn(br, ar),
    added: onlyIn(ar, br),
  };
}
