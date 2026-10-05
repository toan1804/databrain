// Actions on query outputs (handles like r12, names like revenue).

import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { api, toError } from "./lib/api";
import type { OutputInfo } from "./lib/types";
import { formatCount } from "./lib/util";
import { DUCKDB_HEADER, mentionToken, outputLabel, outputRef, withDuckdbHeader } from "./lib/dataflow";

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

export { DUCKDB_HEADER, withDuckdbHeader };

/** New tab on the Results connection with `sql`, optionally run at once. */
export async function openResultsQuery(sql: string, title: string, run = true) {
  sql = withDuckdbHeader(sql);
  try {
    const conn = await resultsConnection();
    const st = useStore.getState();
    const id = st.newTab({ title, sql, connection_id: conn });
    if (run) void st.runTab(id, "all", { doc: sql, selFrom: 0, selTo: 0, cursor: 0 });
  } catch (e) {
    useStore.getState().toast(toError(e).message, "error");
  }
}

/** SQL that queries an output. */
export function outputQuerySql(o: OutputInfo): string {
  return withDuckdbHeader(`SELECT *\nFROM ${outputRef(o)}\nLIMIT 1000;`);
}

export function queryOutput(o: OutputInfo) {
  void openResultsQuery(outputQuerySql(o), `Query ${outputLabel(o)}`, false);
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

/** Ask, then drop an output (its data, saved copy and handle). */
export function dropOutput(o: OutputInfo) {
  const st = useStore.getState();
  const label = outputLabel(o);
  st.askConfirm({
    title: `Drop output ${label}?`,
    reasons: [
      `Frees its ${formatCount(o.rows)} rows; ${outputRef(o)} can no longer be queried.`,
      ...(o.pinned ? ["It is pinned: its saved copy on disk is deleted too."] : []),
      ...(o.name ? ["The previous version of this name (if any) becomes current."] : []),
      "The query in history is kept, so you can run it again.",
    ],
    confirmLabel: "Drop",
    onConfirm: async () => {
      try {
        await api.dropOutput(o.handle);
        useStore.getState().forgetOutputs([o.handle]);
        void useStore.getState().refreshOutputs();
        useStore.getState().toast(`Dropped ${label}`, "success");
      } catch (e) {
        useStore.getState().toast(toError(e).message, "error");
      }
    },
  });
}

/** Ask, then drop every output that is neither pinned nor shown in a tab. */
export function dropUnpinnedOutputs() {
  const st = useStore.getState();
  const n = st.outputs.filter((o) => !o.pinned && !o.active).length;
  if (n === 0) {
    st.toast("Nothing to drop: only pinned outputs and the results shown in tabs are left", "info");
    return;
  }
  st.askConfirm({
    title: `Drop ${n} unpinned output${n === 1 ? "" : "s"}?`,
    reasons: ["Keeps pinned outputs and the latest result of each open tab.", "Their queries stay in history."],
    confirmLabel: "Drop",
    onConfirm: async () => {
      try {
        const before = new Set(useStore.getState().outputs.map((o) => o.handle));
        const dropped = await api.dropUnpinnedOutputs();
        await useStore.getState().refreshOutputs();
        const now = new Set(useStore.getState().outputs.map((o) => o.handle));
        useStore.getState().forgetOutputs([...before].filter((h) => !now.has(h)));
        useStore.getState().toast(`Dropped ${dropped} output${dropped === 1 ? "" : "s"}`, "success");
      } catch (e) {
        useStore.getState().toast(toError(e).message, "error");
      }
    },
  });
}

/** Put `@name` in the assistant input. */
export function mentionInAi(o: OutputInfo) {
  useAi.getState().setOpen(true, "chat");
  // The panel may still be mounting: retry until its listener takes it.
  const detail: { text: string; mode: "chat"; append: boolean; handled?: boolean } = { text: `${mentionToken(o)} `, mode: "chat", append: true };
  let tries = 0;
  const attempt = () => {
    window.dispatchEvent(new CustomEvent("db:ai-prefill", { detail }));
    if (!detail.handled && ++tries < 20) setTimeout(attempt, 50);
  };
  setTimeout(attempt, 0);
}

export function formatAge(ms: number): string {
  const d = Date.now() - ms;
  if (d < 60_000) return "just now";
  if (d < 3_600_000) return `${Math.floor(d / 60_000)}m ago`;
  if (d < 86_400_000) return `${Math.floor(d / 3_600_000)}h ago`;
  return new Date(ms).toLocaleDateString();
}
