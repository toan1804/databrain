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
import type { ConnectorKind, CostEstimate, ParamValue } from "./lib/types";
import { isQueryTab, useStore } from "./store";

/**
 * Each estimate is of one statement on one connection with one set of
 * parameters (`key`): a tab switched to another connection, or a reply
 * that comes back after the tab moved on, never shows a stale number.
 */
interface CostBase {
  sql: string;
  auto: boolean;
  connectionId: string;
  /** costKey(connectionId, sql, params) of the request. */
  key: string;
}

export type CostState =
  | (CostBase & { status: "loading" })
  | (CostBase & { status: "ready"; estimate: CostEstimate })
  | (CostBase & { status: "error"; message: string });

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

export function costKey(connectionId: string, sql: string, params: Record<string, ParamValue> = {}): string {
  const sorted = Object.keys(params)
    .sort()
    .map((k) => [k, params[k]]);
  return JSON.stringify([connectionId, sql, sorted]);
}

/** Drop the tab's estimate (and its "hidden" mark) unless it is of `connectionId`. */
function clearUnlessOn(tabId: string, connectionId: string | null | undefined) {
  const c = useCost.getState();
  const cur = c.byTab[tabId];
  if (!cur || cur.connectionId === connectionId) return;
  useCost.setState({ byTab: { ...c.byTab, [tabId]: undefined }, hidden: { ...c.hidden, [tabId]: undefined } });
}

// A tab that changes connection (or closes) loses its estimate at once,
// whether or not the new connection can estimate at all.
useStore.subscribe((s, prev) => {
  if (s.tabs === prev.tabs) return;
  const byTab = useCost.getState().byTab;
  for (const [tabId, cur] of Object.entries(byTab)) {
    if (!cur) continue;
    clearUnlessOn(tabId, s.tabs.find((t) => t.id === tabId)?.connection_id);
  }
});

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
  clearUnlessOn(tabId, conn?.id);
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
  const params = auto ? (useStore.getState().params[tabId] ?? {}) : await st.paramsFor(tabId, kind, sql);
  if (!params) return;
  const key = costKey(conn.id, sql, params);
  const cur = useCost.getState().byTab[tabId];
  if (auto && cur && cur.key === key && cur.status !== "error") return;
  // The tab may have moved to another connection while we asked for the statement or parameters.
  if (useStore.getState().tabs.find((t) => t.id === tabId)?.connection_id !== conn.id) return;
  const base = { sql, auto, connectionId: conn.id, key };
  const req: CostState = { ...base, status: "loading" };
  useCost.getState().set(tabId, req);
  // Only the latest request of the tab may land, and only while the tab is still on its connection.
  const current = () => useCost.getState().byTab[tabId] === req && useStore.getState().tabs.find((t) => t.id === tabId)?.connection_id === conn.id;
  try {
    const estimate = await api.estimateCost(conn.id, tabId, sql, params);
    if (current()) useCost.getState().set(tabId, { ...base, status: "ready", estimate });
  } catch (e) {
    if (current()) useCost.getState().set(tabId, { ...base, status: "error", message: toError(e).message });
  }
}
