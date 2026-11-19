// Table explorer, step 3: what relates to a table (foreign keys in and out,
// learned and documented joins) and how it is used (DataBrain's own query
// history, job steps). All of it is read locally or from the explorer's
// catalog reads; nothing here is sent to the AI.
import type { ForeignKey } from "./bindings/ForeignKey";
import type { HistoryEntry } from "./bindings/HistoryEntry";
import type { LearnedJoinView } from "./bindings/LearnedJoinView";
import type { Origin } from "./bindings/Origin";
import type { TableColumns } from "./bindings/TableColumns";

const lower = (s: string) => s.toLowerCase();
const lastPart = (schema: string) => schema.slice(schema.lastIndexOf(".") + 1);

/**
 * Whether `full` (a table as the knowledge base names it: `schema.name`, or
 * `catalog.schema.name`) is `schema.name` of the tab (any case; the tab's
 * schema may be `catalog.schema`).
 */
export function sameTable(full: string, schema: string, name: string): boolean {
  const f = lower(full);
  if (f === lower(`${schema}.${name}`)) return true;
  const parts = f.split(".");
  return parts.length >= 2 && parts[parts.length - 1] === lower(name) && parts[parts.length - 2] === lower(lastPart(schema));
}

/** A foreign key of another table that points at this one. */
export type InboundKey = { table: string; fk: ForeignKey };

/**
 * The foreign keys in `tables` (one schema's, as the ER diagram reads them)
 * that point at `schema.name`. Keys in other schemas aren't seen.
 */
export function inboundKeys(tables: TableColumns[], schema: string, name: string): InboundKey[] {
  const out: InboundKey[] = [];
  for (const t of tables) {
    for (const fk of t.foreign_keys) {
      const sameSchema = !fk.ref_schema || lower(fk.ref_schema) === lower(schema) || lower(fk.ref_schema) === lower(lastPart(schema));
      if (sameSchema && lower(fk.ref_table) === lower(name)) out.push({ table: t.table, fk });
    }
  }
  return out;
}

/** A join with this table, seen from its side. */
export type TableJoin = { join: LearnedJoinView; columns: string[]; other: string; otherColumns: string[] };

/** The learned or documented joins that have `schema.name` on one side. */
export function joinsOf(joins: LearnedJoinView[], schema: string, name: string): TableJoin[] {
  const out: TableJoin[] = [];
  for (const j of joins) {
    if (sameTable(j.left_table, schema, name)) out.push({ join: j, columns: j.left_columns, other: j.right_table, otherColumns: j.right_columns });
    else if (sameTable(j.right_table, schema, name)) out.push({ join: j, columns: j.right_columns, other: j.left_table, otherColumns: j.left_columns });
  }
  return out;
}

/** One query (its text, spacing and case aside) run on the table. */
export type QueryUse = { sql: string; runs: number; last: number; origins: Origin[]; failed: number };

const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/**
 * Whether `sql` names the table: its name as a whole word, alone or after
 * a dot (`orders`, `sales.orders`, `"orders"`), any case. A text match: a
 * column or alias of the same name counts too, so it says "mentions".
 */
export function mentions(sql: string, name: string): boolean {
  return new RegExp(`(^|[^A-Za-z0-9_$])${escape(name)}($|[^A-Za-z0-9_$])`, "i").test(sql);
}

/**
 * History entries that mention the table, grouped by query (the same text,
 * spacing and case aside): most runs first, then the latest.
 */
export function queryUses(entries: HistoryEntry[], name: string): QueryUse[] {
  const by = new Map<string, QueryUse>();
  for (const h of entries) {
    if (!mentions(h.sql, name)) continue;
    const key = lower(h.sql.replace(/\s+/g, " ").trim().replace(/;$/, ""));
    const u = by.get(key) ?? { sql: h.sql.trim(), runs: 0, last: 0, origins: [], failed: 0 };
    u.runs += 1;
    if (h.status === "error") u.failed += 1;
    if (h.started_at > u.last) {
      u.last = h.started_at;
      u.sql = h.sql.trim();
    }
    if (!u.origins.includes(h.origin)) u.origins.push(h.origin);
    by.set(key, u);
  }
  return [...by.values()].sort((a, b) => b.runs - a.runs || b.last - a.last);
}
