// Query tips for slow statements: once a statement has run longer than the
// slow-query threshold (Settings → Queries, default 1 minute), look at the
// tables it uses (indexes, partitions, cluster keys) and suggest how to make
// it cheaper. Computed while it is still running, on the connection's
// metadata session (not the busy one), and refreshed when the run ends.
// Shown above the result and as underlines on the statement.
import { api } from "./lib/api";
import { analyzeStatement, refsWithPos, type HintLevel, type LayoutRef } from "./lib/queryHints";
import type { ConnectorKind, PlannedStatement, TableLayout } from "./lib/types";

export interface RunTip {
  statementIndex: number;
  /** Document offsets of the highlighted text (statement start + offset). */
  from: number;
  to: number;
  level: HintLevel;
  message: string;
}

/** A table used by the run, for the editor's hover card. */
export interface RunTable {
  from: number;
  to: number;
  title: string;
  layout: TableLayout;
}

export interface RunTips {
  tips: RunTip[];
  tables: RunTable[];
  /** Statements analysed (their SQL as run, to check the editor still shows it). */
  statements: { index: number; start: number; sql: string }[];
  /** Tables whose layout could not be read. */
  unavailable: { table: string; error: string }[];
}

type Fetch = (connId: string, tables: string[]) => Promise<{ written: string; layout?: TableLayout | null; error?: string | null }[]>;

const TTL = 5 * 60_000;
const cache = new Map<string, { at: number; layout: TableLayout | null; error?: string }>();

/** Layouts for references as written, cached per connection for a few minutes. */
async function layoutsFor(connId: string, written: string[], fetch: Fetch): Promise<Map<string, { layout: TableLayout | null; error?: string }>> {
  const key = (w: string) => `${connId}|${w.toLowerCase()}`;
  const now = Date.now();
  const missing = [...new Set(written)].filter((w) => {
    const c = cache.get(key(w));
    return !c || now - c.at > TTL;
  });
  if (missing.length) {
    const got = await fetch(connId, missing);
    for (const g of got) cache.set(key(g.written), { at: now, layout: g.layout ?? null, error: g.error ?? undefined });
  }
  const out = new Map<string, { layout: TableLayout | null; error?: string }>();
  for (const w of written) {
    const c = cache.get(key(w));
    if (c) out.set(w, c);
  }
  return out;
}

export function clearTipCache() {
  cache.clear();
}

/** Statements worth tips: reads (and changes that filter rows) that finished. */
/** Default slow-query threshold (Settings → Queries). */
export const DEFAULT_SLOW_QUERY_SECONDS = 60;

function analysable(s: { plan: PlannedStatement; status: string }): boolean {
  const k = s.plan.classification.kind;
  return (s.status === "done" || s.status === "running") && (k === "read" || k === "dml");
}

/**
 * Statements that count as slow at `now`: finished after running at least
 * `thresholdMs`, or still running for that long. A threshold of 0 or less
 * turns tips off.
 */
export function slowStatements<T extends { status: string; durationMs?: number; startedAt?: number }>(statements: T[], thresholdMs: number, now: number): T[] {
  if (!(thresholdMs > 0)) return [];
  return statements.filter((s) =>
    s.status === "running" ? s.startedAt !== undefined && now - s.startedAt >= thresholdMs : s.status === "done" && (s.durationMs ?? 0) >= thresholdMs,
  );
}

/**
 * Tips for the finished statements of a run. Tables are resolved by the
 * backend (knowledge index, then the live connection), so this works whether
 * or not the explorer has loaded them.
 */
export async function computeRunTips(
  connId: string,
  kind: ConnectorKind | undefined,
  statements: { plan: PlannedStatement; status: string }[],
  fetch: Fetch = (c, t) => api.hintLayouts(c, t),
): Promise<RunTips> {
  const out: RunTips = { tips: [], tables: [], statements: [], unavailable: [] };
  const todo = statements.filter(analysable).slice(0, 20);
  const refs = todo.map((s) =>
    refsWithPos(s.plan.sql).filter((r) => !(kind === "duckdb" && r.parts[0]?.toLowerCase() === "results")),
  );
  const written = refs.flat().map((r) => r.parts.join("."));
  if (!written.length) return out;
  const layouts = await layoutsFor(connId, written, fetch);
  const reported = new Set<string>();
  todo.forEach((s, i) => {
    const known: LayoutRef[] = [];
    for (const ref of refs[i]) {
      const w = ref.parts.join(".");
      const l = layouts.get(w);
      if (l?.layout) {
        known.push({ ref, layout: l.layout });
        out.tables.push({ from: s.plan.start + ref.from, to: s.plan.start + ref.to, title: w, layout: l.layout });
      } else if (l?.error && !reported.has(w.toLowerCase())) {
        reported.add(w.toLowerCase());
        out.unavailable.push({ table: w, error: l.error });
      }
    }
    out.statements.push({ index: s.plan.index, start: s.plan.start, sql: s.plan.sql });
    for (const h of analyzeStatement(s.plan.sql, known, kind)) {
      out.tips.push({ statementIndex: s.plan.index, from: s.plan.start + h.from, to: s.plan.start + h.to, level: h.level, message: h.message });
    }
  });
  return out;
}
