// Knowledge index scope: which catalogs/schemas to index (pure, tested).
import type { ConnectorKind, IndexPlan, PlanSchema } from "./types";

export interface CatalogRow {
  /** Catalog name, or "" for engines without catalogs. */
  name: string;
  schemas: PlanSchema[];
  objects: number | null;
}

/** Group plan schemas by catalog; system schemas only when `withSystem`. */
export function planCatalogs(plan: IndexPlan, withSystem = false): CatalogRow[] {
  const map = new Map<string, CatalogRow>();
  for (const s of plan.schemas) {
    if (s.system && !withSystem) continue;
    const k = s.catalog ?? "";
    let row = map.get(k);
    if (!row) map.set(k, (row = { name: k, schemas: [], objects: s.objects === null ? null : 0 }));
    row.schemas.push(s);
    if (row.objects !== null) row.objects = s.objects === null ? null : row.objects + s.objects;
  }
  return [...map.values()];
}

/** Initial selection: the saved scope, or (never chosen) the default schemas. */
export function initialSelection(plan: IndexPlan): Set<string> {
  if (plan.scope.length > 0) return new Set(plan.schemas.filter((s) => s.selected).map((s) => s.name));
  // Large and never chosen: start with the session's default schema only.
  if (plan.large) {
    const d = plan.schemas.filter((s) => s.is_default && !s.system);
    if (d.length) return new Set(d.map((s) => s.name));
  }
  return new Set(plan.schemas.filter((s) => s.selected).map((s) => s.name));
}

/**
 * Scope entries for the backend: `*` when every non-system schema is chosen,
 * `catalog.*` for fully chosen catalogs (so new schemas there are picked up
 * on re-index), otherwise schema ids.
 */
export function buildScope(plan: IndexPlan, selected: Set<string>): string[] {
  const user = plan.schemas.filter((s) => !s.system);
  const sys = plan.schemas.filter((s) => s.system && selected.has(s.name)).map((s) => s.name);
  if (user.length > 0 && user.every((s) => selected.has(s.name))) return ["*", ...sys];
  const out: string[] = [];
  for (const c of planCatalogs(plan)) {
    const chosen = c.schemas.filter((s) => selected.has(s.name));
    if (!chosen.length) continue;
    if (c.name && chosen.length === c.schemas.length && c.schemas.length > 1) out.push(`${c.name}.*`);
    else out.push(...chosen.map((s) => s.name));
  }
  return [...out, ...sys];
}

const BULK: ConnectorKind[] = ["databricks"];

export const DEFAULT_BATCH = 25;
export const MAX_BATCH = 500;
/** Databricks puts at most this many schemas in one `IN (…)` list. */
const IN_LIST = 200;

/** Engines that fetch metadata per catalog in bulk (see `Session::bulk_metadata`). */
export const isBulk = (kind: ConnectorKind) => BULK.includes(kind);

/**
 * Rough number of metadata queries an index run will send. Mirrors the
 * backend: schemas are processed in batches of `batch` (in plan order);
 * bulk engines send 2 queries (tables + columns) per catalog in a batch.
 */
export function estimateQueries(kind: ConnectorKind, plan: IndexPlan, selected: Set<string>, batch = DEFAULT_BATCH): number {
  const chosen = plan.schemas.filter((s) => selected.has(s.name));
  if (!isBulk(kind)) return chosen.length * 2;
  const size = Math.min(Math.max(1, Math.floor(batch) || DEFAULT_BATCH), MAX_BATCH);
  let n = 0;
  for (let i = 0; i < chosen.length; i += size) {
    const perCat = new Map<string, number>();
    for (const s of chosen.slice(i, i + size)) perCat.set(s.catalog ?? "", (perCat.get(s.catalog ?? "") ?? 0) + 1);
    for (const v of perCat.values()) n += Math.ceil(v / IN_LIST) * 2;
  }
  return n;
}

export function selectedObjects(plan: IndexPlan, selected: Set<string>): number | null {
  let n = 0;
  for (const s of plan.schemas) {
    if (!selected.has(s.name)) continue;
    if (s.objects === null) return null;
    n += s.objects;
  }
  return n;
}
