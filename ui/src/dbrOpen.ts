// `dbr` without -i: what it hands the running app. Data files open as
// results (as "Query a file…" opens them); `.sql` files open in their file
// tabs (run with `--run`), SQL in new tabs that run it; on `-c`'s connection,
// else an open file keeps its own, else Local files. They run as the editor
// runs them: the read-only setting and the production confirmation apply.
// An open file runs what its editor holds, saved or not.
import { api, isTauri, onDbrOpen, toError } from "./lib/api";
import type { DbrOpen } from "./lib/bindings/DbrOpen";
import { editorBridge } from "./editorBridge";
import { localFilesConnection, openLocalFile } from "./components/sidebar/queryLocalFile";
import { useStore } from "./store";
import { isQueryTab } from "./store/types";

/** Open what one `dbr` command asked for, each item in its own tab. */
export async function applyDbrOpen(o: DbrOpen) {
  const st = useStore.getState();
  const connId = o.connection_id;
  if (connId && !st.connections.some((c) => c.id === connId)) await st.refreshConnections();
  const conn = connId ? useStore.getState().connections.find((c) => c.id === connId) : undefined;
  if (connId && !conn) {
    st.toast("dbr: its connection is no longer saved", "error");
    return;
  }
  for (const item of o.items) {
    try {
      if (item.kind === "data") {
        await openLocalFile(item.path);
        continue;
      }
      let id: string | undefined;
      if (item.kind === "sql_file") {
        id = await useStore.getState().openSqlFile(item.path);
        if (!id) continue;
      }
      const open = id ? useStore.getState().tabs.find((t) => t.id === id) : undefined;
      // -c's connection; else an open file's own; else Local files (for
      // what runs: a file opened without --run is left without one).
      const runs = item.kind === "sql" || o.run;
      const own = isQueryTab(open) ? open.connection_id : null;
      const target = conn?.id ?? own ?? (runs ? (await localFilesConnection())?.id : null);
      if (!target && runs) {
        useStore.getState().toast("dbr: the Local files connection couldn't be saved", "error");
        continue;
      }
      if (item.kind === "sql") id = useStore.getState().newTab({ sql: item.text, connection_id: target });
      if (!id) continue;
      const s = useStore.getState();
      const tab = s.tabs.find((t) => t.id === id);
      if (!isQueryTab(tab)) continue;
      if (target && tab.connection_id !== target) {
        if (tab.connection_id) s.toast(`dbr: ${tab.title} is now on ${conn?.name ?? "Local files"}`, "info");
        s.updateTab(id, { connection_id: target });
      }
      for (const [name, v] of Object.entries(o.params)) if (v) s.setParam(id, name, v);
      if (!runs) continue;
      if (s.runs[id]?.running) {
        s.toast(`dbr: ${tab.title} is already running`, "info");
        continue;
      }
      const doc = editorBridge.get(id)?.state.doc.toString() ?? tab.sql;
      await s.runTab(id, "all", { doc, selFrom: 0, selTo: 0, cursor: 0 });
    } catch (e) {
      useStore.getState().toast(`dbr: ${toError(e).message}`, "error");
    }
  }
}

let taking: Promise<void> = Promise.resolve();

/** Take what `dbr` left (one request at a time, in order). */
function takeAll() {
  taking = taking.then(async () => {
    const opens = await api.takeDbrOpens().catch(() => []);
    for (const o of opens) await applyDbrOpen(o);
  });
  return taking;
}

/**
 * Listen for `dbr` (after the app has loaded), then take what it sent while
 * the app was starting.
 */
export async function startDbrOpens() {
  if (!isTauri()) return;
  await onDbrOpen(() => void takeAll());
  await takeAll();
}
