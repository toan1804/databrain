// Pure helpers for output references, @mentions and notebook dataflow.

import type { NotebookCell, OutputInfo } from "./types";

/** `results.revenue`, `results.revenue__1` or `results.r12`. */
export function outputRef(o: Pick<OutputInfo, "handle" | "name" | "version_of">): string {
  const t = o.name ?? (o.version_of ? `${o.version_of[0]}__${o.version_of[1]}` : o.handle);
  return `results.${/^[a-z_][a-z0-9_]*$/.test(t) ? t : `"${t.replace(/"/g, '""')}"`}`;
}

/** Short label: name (or name~k for old versions), else handle. */
export function outputLabel(o: Pick<OutputInfo, "handle" | "name" | "version_of">): string {
  return o.name ?? (o.version_of ? `${o.version_of[0]}~${o.version_of[1]}` : o.handle);
}

/** Mention token used in the AI chat. */
export function mentionToken(o: Pick<OutputInfo, "handle" | "name">): string {
  return `@${o.name ?? o.handle}`;
}

/** Mentions in a chat message that match known outputs. */
export function extractMentions(text: string, outputs: OutputInfo[]): string[] {
  const out: string[] = [];
  for (const m of text.matchAll(/(^|[\s(,])@([A-Za-z_][\w]*)/g)) {
    const t = m[2];
    const known = outputs.some((o) => o.handle.toLowerCase() === t.toLowerCase() || (o.name ?? "").toLowerCase() === t.toLowerCase());
    if (known && !out.some((x) => x.toLowerCase() === t.toLowerCase())) out.push(t);
  }
  return out;
}

/** Output names referenced as results.<name> in SQL (comments/strings ignored, roughly). */
/** First line of SQL written for outputs, so it is clear which engine runs it. */
export const DUCKDB_HEADER = "-- Runs locally on DuckDB (Results connection): outputs are results.<name>, DuckDB SQL syntax.";

/** `sql` with the DuckDB header line (once). */
export function withDuckdbHeader(sql: string): string {
  return sql.startsWith(DUCKDB_HEADER) ? sql : `${DUCKDB_HEADER}\n${sql}`;
}

export function referencedOutputs(sql: string): string[] {
  const clean = sql.replace(/--[^\n]*/g, "").replace(/\/\*[\s\S]*?\*\//g, "").replace(/'(?:[^']|'')*'/g, "''");
  const out: string[] = [];
  for (const m of clean.matchAll(/(?<![\w.])results\s*\.\s*(?:"((?:[^"]|"")+)"|([A-Za-z_]\w*))/gi)) {
    const n = (m[1] ?? m[2]).replace(/""/g, '"');
    if (!out.some((x) => x.toLowerCase() === n.toLowerCase())) out.push(n);
  }
  return out;
}

export interface CellDeps {
  /** Output names this cell reads (results.<name>). */
  reads: string[];
  /** Earlier cells producing those names. */
  producers: string[];
  /** Later cells reading this cell's output. */
  dependents: string[];
  /** A producer ran after this cell last ran. */
  stale: boolean;
  /** Referenced names with no producer and no known output. */
  missing: string[];
}

/** Dataflow between cells via named outputs. */
export function cellDeps(cells: NotebookCell[], knownOutputs: string[]): Record<string, CellDeps> {
  const known = new Set(knownOutputs.map((n) => n.toLowerCase()));
  const out: Record<string, CellDeps> = {};
  const producerOf = (name: string, before: number) => {
    for (let i = before - 1; i >= 0; i--) {
      const c = cells[i];
      if (c.kind === "sql" && c.output_name && c.output_name.toLowerCase() === name.toLowerCase()) return c;
    }
    return undefined;
  };
  cells.forEach((c, i) => {
    const reads = c.kind === "sql" ? referencedOutputs(c.source).map((n) => n.replace(/__\d+$/, "")) : [];
    const producers = reads.map((r) => producerOf(r, i)).filter((p): p is NotebookCell => !!p);
    const ranAt = c.last_run?.finished_at ?? 0;
    out[c.id] = {
      reads,
      producers: producers.map((p) => p.id),
      dependents: [],
      stale: ranAt > 0 && producers.some((p) => (p.last_run?.finished_at ?? 0) > ranAt && !p.last_run?.error),
      missing: reads.filter((r) => !producerOf(r, i) && !known.has(r.toLowerCase()) && !/^r\d+$/i.test(r)),
    };
  });
  for (const c of cells) for (const p of out[c.id].producers) out[p]?.dependents.push(c.id);
  return out;
}
