// `dbr` without -i: what it hands the running app. Data files open as
// results (as "Query a file…" opens them); `.sql` files and SQL open in tabs
// on the connection (`-c`, else Local files) and run as the editor runs
// them: the read-only setting and the production confirmation apply.
import { api, isTauri, onDbrOpen, toError } from "./lib/api";
import type { DbrOpen } from "./lib/bindings/DbrOpen";
import { editorBridge } from "./editorBridge";
import { localFilesConnection, openLocalFile } from "./components/sidebar/queryLocalFile";
import { useStore } from "./store";
import { isQueryTab } from "./store/types";

/** Open what one `dbr` command asked for, each item in its own tab. */
export async function applyDbrOpen(o: DbrOpen) {
  const st = useStore.getState();
  let connId = o.connection_id;
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
      connId = conn?.id ?? (await localFilesConnection())?.id ?? null;
      if (!connId) return;
      const id = item.kind === "sql_file" ? await useStore.getState().openSqlFile(item.path) : useStore.getState().newTab({ sql: item.text, connection_id: connId });
      if (!id) continue;
      const s = useStore.getState();
      s.updateTab(id, { connection_id: connId });
      for (const [name, v] of Object.entries(o.params)) if (v) s.setParam(id, name, v);
      const tab = s.tabs.find((t) => t.id === id);
      if (!isQueryTab(tab)) continue;
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
