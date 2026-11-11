// How much of a connection the knowledge index covers, for the AI panel's
// hint: nothing indexed, or far fewer tables than the explorer lists.
import type { DbObject, KnState } from "./types";

export type Coverage = { kind: "none" } | { kind: "few"; indexed: number; total: number };

/** Below this many tables in the explorer, a partial index isn't worth a hint. */
export const FEW_MIN_TABLES = 20;
/** Indexed share below which the index counts as "few". */
export const FEW_SHARE = 0.5;

/**
 * Schemas the knowledge index leaves out unless chosen (as `system_schema`
 * in crates/ai/src/knowledge.rs): catalog schemas such as
 * `information_schema` and `pg_catalog`.
 */
export function systemSchema(name: string): boolean {
  const last = name.toLowerCase().split(".").pop() ?? "";
  return (
    ["information_schema", "pg_catalog", "pg_toast", "sys", "mysql", "performance_schema", "sysibm", "ctxsys", "mdsys", "xdb", "outln"].includes(last) ||
    last.startsWith("pg_temp")
  );
}

/** A schema id in an index scope (as `scope_matches` in knowledge.rs): `*`, the id, or `catalog.*`. */
export function inScope(scope: string[], schema: string): boolean {
  const s = schema.toLowerCase();
  return scope.some((w) => {
    const e = w.trim().toLowerCase();
    return e === "*" || e === s || (e.endsWith(".*") && s.startsWith(e.slice(0, -1)) && s.length > e.length - 1);
  });
}

/**
 * Tables and views the explorer has listed for a connection that the index
 * would hold (not functions, procedures or sequences; not system schemas;
 * only the chosen schemas when the connection's index scope is set); `null`
 * when it has listed none yet.
 */
export function explorerTables(objects: Record<string, DbObject[]>, connId: string, scope: string[] = []): number | null {
  // `*` alone means every schema except system ones (as the indexer reads it).
  if (scope.length && scope.every((w) => w.trim() === "*")) scope = [];
  let n = 0;
  let seen = false;
  for (const [key, list] of Object.entries(objects)) {
    if (!key.startsWith(`${connId}|`)) continue;
    const schema = key.slice(connId.length + 1);
    if (scope.length ? !inScope(scope, schema) : systemSchema(schema)) continue;
    seen = true;
    n += list.filter((o) => !["function", "procedure", "sequence", "package"].includes(o.kind)).length;
  }
  return seen ? n : null;
}

/** The hint to show, or null when the index looks complete enough. */
export function indexCoverage(state: KnState | null, total: number | null): Coverage | null {
  const indexed = state?.objects ?? 0;
  if (indexed === 0) return { kind: "none" };
  if (total !== null && total >= FEW_MIN_TABLES && indexed < total * FEW_SHARE) return { kind: "few", indexed, total };
  return null;
}
