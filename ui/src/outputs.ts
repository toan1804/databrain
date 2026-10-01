// Actions on query outputs (handles like r12, names like revenue).

import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { api, toError } from "./lib/api";
import type { OutputInfo } from "./lib/types";
import { mentionToken, outputLabel, outputRef } from "./lib/dataflow";

export { extractMentions, mentionToken, outputLabel, outputRef, referencedOutputs } from "./lib/dataflow";
import { useStore } from "./store";
import { useAi } from "./aiStore";

let resultsConnId: string | null = null;

/** Id of the local "Results (DuckDB)" connection (created on first use). */
export async function resultsConnection(): Promise<string> {
  if (resultsConnId && useStore.getState().connections.some((c) => c.id === resultsConnId)) return resultsConnId;
  resultsConnId = await api.resultsConnection();
  if (!useStore.getState().connections.some((c) => c.id === resultsConnId)) await useStore.getState().refreshConnections();
  return resultsConnId;
}

export function isResultsConnection(id: string | null | undefined): boolean {
  const c = useStore.getState().connections.find((x) => x.id === id);
  return !!c && c.config.kind === "duckdb" && c.config.options?.databrain_results === "1";
}

/** New tab on the Results connection with `sql`, optionally run at once. */
export async function openResultsQuery(sql: string, title: string, run = true) {
  try {
    const conn = await resultsConnection();
    const st = useStore.getState();
    const id = st.newTab({ title, sql, connection_id: conn });
    if (run) void st.runTab(id, "all", { doc: sql, selFrom: 0, selTo: 0, cursor: 0 });
  } catch (e) {
    useStore.getState().toast(toError(e).message, "error");
  }
}

export function queryOutput(o: OutputInfo) {
  void openResultsQuery(`SELECT *\nFROM ${outputRef(o)}\nLIMIT 1000;`, `Query ${outputLabel(o)}`, false);
}

/** Open the stored data in an output viewer tab (no re-run). */
export async function openOutput(o: OutputInfo) {
  const st = useStore.getState();
  try {
    if (o.state === "evicted") {
      st.toast(`${o.handle} was freed from memory. Re-run its query to get it back.`, "info");
      return;
    }
    await api.loadOutput(o.handle);
    const existing = st.tabs.find((t) => t.output_ref === o.handle);
    if (existing) return st.setActiveTab(existing.id);
    st.newTab({ title: `${outputLabel(o)} (${o.handle})`, sql: "", output_ref: o.handle, connection_id: o.connection_id });
  } catch (e) {
    st.toast(toError(e).message, "error");
  }
}

export async function copyOutputRef(o: OutputInfo) {
  await writeText(outputRef(o));
  useStore.getState().toast(`Copied ${outputRef(o)}`, "success");
}

export async function renameOutput(o: OutputInfo, name: string | null) {
  try {
    const n = await api.renameOutput(o.handle, name);
    useStore.getState().applyOutput(n);
    void useStore.getState().refreshOutputs();
    return n;
  } catch (e) {
    useStore.getState().toast(toError(e).message, "error");
    return null;
  }
}

export async function togglePin(o: OutputInfo) {
  try {
    const n = await api.pinOutput(o.handle, !o.pinned);
    useStore.getState().applyOutput(n);
    void useStore.getState().refreshOutputs();
    useStore.getState().toast(n.pinned ? `Pinned ${outputLabel(n)} — kept across restarts` : `Unpinned ${outputLabel(n)}`, "success");
  } catch (e) {
    useStore.getState().toast(toError(e).message, "error");
  }
}

/** Put `@name` in the assistant input. */
export function mentionInAi(o: OutputInfo) {
  useAi.getState().setOpen(true, "chat");
  setTimeout(
    () => window.dispatchEvent(new CustomEvent("db:ai-prefill", { detail: { text: `${mentionToken(o)} `, mode: "chat", append: true } })),
    30,
  );
}

export function formatAge(ms: number): string {
  const d = Date.now() - ms;
  if (d < 60_000) return "just now";
  if (d < 3_600_000) return `${Math.floor(d / 60_000)}m ago`;
  if (d < 86_400_000) return `${Math.floor(d / 3_600_000)}h ago`;
  return new Date(ms).toLocaleDateString();
}
