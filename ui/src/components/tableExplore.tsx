// The table explorer's reads (profile, statistics, counts, constraints,
// history): each one read on a session of the table tab's own, through
// `api.exploreQuery` (origin `explorer`: one read, in the history), asking
// first where it reads every row. Shared by the parts in TableTab.tsx and
// TableHistory.tsx.
import { useEffect, useRef, useState } from "react";
import { FileCode2 } from "lucide-react";
import { api, toError } from "../lib/api";
import { RESULTS_SCHEMA_ID } from "../lib/catalog";
import { billsScans, exploreKey, readAllReasons } from "../lib/explore";
import type { ConnectionView, ExploreRows } from "../lib/types";
import { useStore, type DdlRef } from "../store";
import { objectRef } from "./CatalogSearch";

/** What the profile, statistics and count parts need to run their reads. */
export type Explore = { tabId: string; conn: ConnectionView; reference: DdlRef; rowEstimate: number | null; asked: { current: boolean } };

/** How a DuckDB result or attached file is named (as Query names it), else undefined. */
export function localFrom(e: Explore): { from?: string; local?: "results" | "files" } {
  const { conn, reference } = e;
  if (conn.config.kind !== "duckdb") return {};
  const local = reference.schema === RESULTS_SCHEMA_ID ? "results" : reference.schema.endsWith(".files") ? "files" : undefined;
  return local ? { local, from: objectRef(conn, { schema: reference.schema, name: reference.name, kind: reference.kind }) } : {};
}

export type Ran = { status: "idle" } | { status: "running"; sql: string } | { status: "done"; sql: string; rows: ExploreRows } | { status: "error"; sql: string; message: string };

export const ask = (title: string, reasons: string[], confirmLabel: string) =>
  new Promise<boolean>((resolve) => useStore.getState().askConfirm({ title, reasons, confirmLabel, onConfirm: () => resolve(true), onCancel: () => resolve(false) }));

/**
 * Run one explorer read on session `{tabId}#{what}`. `heavy`: it reads
 * every row, so it asks first ({@link readAllReasons}: BigQuery's bill at
 * each read; production or a large table once per tab). Null when not run.
 * The session closes when the part goes away, which stops what still runs
 * on it (a column's top values closed, the columns read again).
 */
export function useExploreRead(e: Explore, what: string) {
  const [ran, setRan] = useState<Ran>({ status: "idle" });
  const { conn, reference, tabId } = e;
  const kind = conn.config.kind;
  const key = exploreKey(tabId, what);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      // Strict mode unmounts and mounts again at once: only an unmount that stays closes it.
      setTimeout(() => !mounted.current && void api.closeTab(key).catch(() => {}), 0);
    };
  }, [key]);
  const read = async (sql: string, heavy: string | null): Promise<ExploreRows | null> => {
    if (heavy) {
      const estimate = billsScans(kind) ? await api.estimateCost(conn.id, key, sql).catch(() => null) : undefined;
      const reasons = readAllReasons({ kind, prod: conn.env === "prod", connName: conn.name, table: reference.name, rowEstimate: e.rowEstimate, asked: e.asked.current, estimate });
      if (reasons.length) {
        const after = billsScans(kind) ? "BigQuery asks at each read: each one bills what it scans." : "This tab won't ask again.";
        if (!(await ask(`${heavy} of ${reference.name}?`, [...reasons, after], "Read it"))) return null;
      }
      e.asked.current = true;
    }
    setRan({ status: "running", sql });
    try {
      const rows = await api.exploreQuery(conn.id, key, sql);
      setRan({ status: "done", sql, rows });
      return rows;
    } catch (err) {
      setRan({ status: "error", sql, message: toError(err).message });
      return null;
    }
  };
  // Closing the session stops what runs on it.
  const stop = () => void api.closeTab(key).catch(() => {});
  return { ran, setRan, read, stop };
}

/** The SQL a part ran, with Open in editor (every explorer read is in the history as Explorer too). */
export function RanSql({ e, sql, ms }: { e: Explore; sql: string; ms?: number }) {
  return (
    <div className="flex items-start gap-2 border-t border-line px-2 py-1 text-[11px] text-muted">
      <code className="min-w-0 flex-1 truncate font-mono" title={sql}>
        {sql.replace(/\s+/g, " ")}
      </code>
      {ms != null && <span className="shrink-0">{ms} ms</span>}
      <button className="btn-ghost shrink-0 py-0 text-[11px]" onClick={() => useStore.getState().newTab({ title: e.reference.name, sql, connection_id: e.conn.id })}>
        <FileCode2 size={11} /> Open in editor
      </button>
    </div>
  );
}
