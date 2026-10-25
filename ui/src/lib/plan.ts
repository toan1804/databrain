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

/** Rows a step read in all: actual (× loops) when analyzed, else the estimate. */
function rowsOf(n: PlanNode): number | undefined {
  if (n.actual_rows !== undefined) return n.actual_rows * Math.max(1, n.loops ?? 1);
  return n.rows;
}

export function warningsOf(n: PlanNode): string[] {
  const out: string[] = [];
  const rows = rowsOf(n);
  if (FULL_SCAN.test(n.op) && (rows === undefined || rows >= 1000)) out.push(`Full scan${rows !== undefined ? ` of ${fmtNum(rows)} rows` : ""}`);
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
