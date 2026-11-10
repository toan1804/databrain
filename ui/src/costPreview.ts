// Cost preview per tab: what the statement at the cursor (or the selection)
// would scan, asked of the database without running it. BigQuery is asked
// on its own a moment after the editor stops changing (dry runs are free);
// Snowflake and Databricks only when asked ("Cost" in the toolbar), since
// their EXPLAIN uses the warehouse, and a stopped Databricks warehouse starts.
// Shown next to the slow-query tips, above the result.

import { create } from "zustand";
import { editorBridge } from "./editorBridge";
import { api, toError } from "./lib/api";
import { traitsOf } from "./lib/sourceTraits";
import type { ConnectorKind, CostEstimate } from "./lib/types";
import { isQueryTab, useStore } from "./store";

export type CostState =
  | { status: "loading"; sql: string; auto: boolean }
  | { status: "ready"; sql: string; auto: boolean; estimate: CostEstimate; connectionId: string }
  | { status: "error"; sql: string; auto: boolean; message: string };

interface CostStore {
  byTab: Record<string, CostState | undefined>;
  hidden: Record<string, string | undefined>;
  set: (tabId: string, s: CostState | undefined) => void;
  hide: (tabId: string, sql: string) => void;
}

export const useCost = create<CostStore>((set) => ({
  byTab: {},
  hidden: {},
  set: (tabId, s) => set((st) => ({ byTab: { ...st.byTab, [tabId]: s } })),
  hide: (tabId, sql) => set((st) => ({ hidden: { ...st.hidden, [tabId]: sql } })),
}));

/** Engines asked on their own while typing (free, no warehouse). */
export const AUTO_COST_KINDS = new Set(["bigquery"]);

/** The selection, else the statement at the cursor. */
async function statementOf(tabId: string, kind: ConnectorKind): Promise<string> {
  const st = useStore.getState();
  const tab = st.tabs.find((t) => t.id === tabId);
  const view = editorBridge.get(tabId);
  const doc = view?.state.doc.toString() ?? tab?.sql ?? "";
  const sel = view?.state.selection.main;
  const picked = sel && !sel.empty ? doc.slice(sel.from, sel.to) : "";
  if (picked.trim()) return picked;
  const span = await api.statementAtCursor(kind, doc, sel?.head ?? 0).catch(() => null);
  return span?.sql ?? "";
}

/**
 * Estimate the tab's current statement. `auto`: started by typing; it
 * stays quiet (no toast, no prompt for parameters) and skips a statement
 * already estimated.
 */
export async function estimateCost(tabId: string, auto = false): Promise<void> {
  const st = useStore.getState();
  const tab = st.tabs.find((t) => t.id === tabId);
  if (!isQueryTab(tab)) return;
  const conn = st.connections.find((c) => c.id === tab.connection_id);
  if (!conn) {
    if (!auto) st.toast("Choose a connection for this tab first", "error");
    return;
  }
  const kind = conn.config.kind;
  if (!traitsOf(kind).cost_preview) {
    if (!auto) st.toast(`${traitsOf(kind).name} gives no cost estimate before a run: use Plan for its query plan`, "info");
    return;
  }
  const sql = (await statementOf(tabId, kind)).trim();
  if (!sql) {
    if (!auto) st.toast("Nothing to estimate: put the cursor in a statement", "info");
    return;
  }
  const cur = useCost.getState().byTab[tabId];
  if (auto && cur && cur.sql === sql && cur.status !== "error") return;
  const params = auto ? (st.params[tabId] ?? {}) : await st.paramsFor(tabId, kind, sql);
  if (!params) return;
  useCost.getState().set(tabId, { status: "loading", sql, auto });
  try {
    const estimate = await api.estimateCost(conn.id, tabId, sql, params);
    if (useCost.getState().byTab[tabId]?.sql === sql) useCost.getState().set(tabId, { status: "ready", sql, auto, estimate, connectionId: conn.id });
  } catch (e) {
    if (useCost.getState().byTab[tabId]?.sql === sql) useCost.getState().set(tabId, { status: "error", sql, auto, message: toError(e).message });
  }
}
