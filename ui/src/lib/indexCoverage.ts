// How much of a connection the knowledge index covers, for the AI panel's
// hint: nothing indexed, or far fewer tables than the explorer lists.
import type { DbObject, KnState } from "./types";

export type Coverage = { kind: "none" } | { kind: "few"; indexed: number; total: number };

/** Below this many tables in the explorer, a partial index isn't worth a hint. */
export const FEW_MIN_TABLES = 20;
/** Indexed share below which the index counts as "few". */
export const FEW_SHARE = 0.5;

/**
 * Tables and views the explorer has listed for a connection (what the index
 * would hold: not functions, procedures or sequences); `null` when it has
 * listed none yet.
 */
export function explorerTables(objects: Record<string, DbObject[]>, connId: string): number | null {
  let n = 0;
  let seen = false;
  for (const [key, list] of Object.entries(objects)) {
    if (!key.startsWith(`${connId}|`)) continue;
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
