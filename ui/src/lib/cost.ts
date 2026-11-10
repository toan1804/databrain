// Cost preview: reading the database's estimate of what a statement would
// scan before it runs (BigQuery dry run, Snowflake and Databricks EXPLAIN).

import type { CostEstimate } from "./types";

/** Bytes in binary units, as BigQuery and the consoles show them. */
export function fmtBytes(b: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  let v = b;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return i === 0 ? `${v} B` : `${v.toFixed(v >= 100 ? 0 : 1)} ${units[i]}`;
}

/** Scans this large are worth a second look before running. */
export const LARGE_SCAN_BYTES = 10 * 1024 ** 3;

/** One line: what the statement would scan. */
export function costSummary(e: CostEstimate): string {
  const parts: string[] = [];
  if (e.cache_hit) parts.push("from cache, 0 B billed");
  else if (e.bytes != null) parts.push(`${e.method === "EXPLAIN COST" ? "reads about" : "scans"} ${fmtBytes(e.bytes)}`);
  if (e.partitions_assigned != null && e.partitions_total != null) parts.push(`${fmtCount(e.partitions_assigned)} of ${fmtCount(e.partitions_total)} partitions`);
  if (e.rows != null) parts.push(`~${fmtCount(e.rows)} rows`);
  return parts.length ? parts.join(" · ") : "no size estimate";
}

/** `warn` when the run would fail on the byte cap, scan a lot, or prune nothing. */
export function costLevel(e: CostEstimate): "info" | "warn" {
  if (e.over_limit != null) return "warn";
  if (!e.cache_hit && e.bytes != null && e.bytes >= LARGE_SCAN_BYTES) return "warn";
  if (e.partitions_total != null && e.partitions_total > 1 && e.partitions_assigned === e.partitions_total) return "warn";
  return "info";
}

function fmtCount(n: number): string {
  return n.toLocaleString("en-US");
}
