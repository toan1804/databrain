import type { StateCreator, StoreApi } from "zustand";
import { flushDraft, flushDrafts } from "../drafts";
import { api, isTauri, toError } from "../lib/api";
import type { ParamValue } from "../lib/types";
import { uid } from "../lib/util";
import { editorBridge } from "../editorBridge";
import { baseName, persistParams, persistTabs } from "./helpers";
import { isQueryTab, type State, type Tab } from "./types";

const RECENT_FILES = 12;
/** Put `path` first in the recent files (`recent_sql_files` setting). */
function rememberFile(store: StoreApi<State>, path: string) {
  const s = store.getState();
  const recentFiles = [path, ...s.recentFiles.filter((p) => p !== path)].slice(0, RECENT_FILES);
  store.setState({ recentFiles });
  if (isTauri()) void api.setSetting("recent_sql_files", recentFiles).catch(() => {});
}

function nextTabTitle(tabs: Tab[]): string {
  let n = 1;
  const titles = new Set(tabs.map((t) => t.title));
  while (titles.has(`Query ${n}`)) n++;
  return `Query ${n}`;
}

export interface TabsSlice {
  tabs: Tab[];
  activeTabId: string | null;
  /** Recently opened or saved .sql files, newest first. */
  recentFiles: string[];
  /** Open a .sql file in a tab (asks for one without `path`); focuses its tab when open. */
  openSqlFile: (path?: string) => Promise<void>;
  /** Save a tab to its file (asks for a path for other tabs, or with `saveAs`). */
  saveTabFile: (tabId: string, saveAs?: boolean) => Promise<void>;
  /** Re-read files changed by other programs (on window focus). */
  checkFiles: () => Promise<void>;
  newTab: (init?: Partial<Tab>) => string;
  closeTab: (id: string) => void;
  /** Tabs whose next result opens "Load into…" (importing a file). */
  loadIntoOnResult: Record<string, boolean>;
  setLoadIntoOnResult: (tabId: string, on: boolean) => void;
  /** Recently closed tabs, newest last (⌘⇧T reopens them). */
  closedTabs: { tab: Tab; index: number; params?: Record<string, ParamValue> }[];
  /** Reopen the most recently closed tab where it was. False when none. */
  reopenClosedTab: () => boolean;
  setActiveTab: (id: string) => void;
  updateTab: (id: string, patch: Partial<Tab>) => void;
  moveTab: (from: number, to: number) => void;
}

export const createTabsSlice: StateCreator<State, [], [], TabsSlice> = (set, get, store) => ({
  tabs: [],
  closedTabs: [],
  loadIntoOnResult: {},
  setLoadIntoOnResult: (tabId, on) =>
    set((s) => {
      const next = { ...s.loadIntoOnResult };
      if (on) next[tabId] = true;
      else delete next[tabId];
      return { loadIntoOnResult: next };
    }),
  activeTabId: null,
  recentFiles: [],
  openSqlFile: async (path) => {
    const s = get();
    if (!path) {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const picked = await open({ multiple: false, directory: false, filters: [{ name: "SQL", extensions: ["sql", "ddl", "pgsql", "psql", "plsql", "pks", "pkb", "txt"] }, { name: "All files", extensions: ["*"] }] });
      if (typeof picked !== "string") return;
      path = picked;
    }
    const open = s.tabs.find((t) => t.file_path === path);
    if (open) return s.setActiveTab(open.id);
    try {
      const f = await api.readSqlFile(path);
      flushDrafts();
      const active = get().tabs.find((t) => t.id === get().activeTabId);
      // Replace the lone empty scratch tab.
      const lone = s.tabs.length === 1 && isQueryTab(active) && !active.sql.trim() && !active.file_path && !active.saved_query_id;
      const id = s.newTab({ title: baseName(f.path), sql: f.text, file_path: f.path, file_text: f.text, file_mtime: f.mtime_ms });
      if (lone && active) get().closeTab(active.id);
      get().setActiveTab(id);
      rememberFile(store, f.path);
    } catch (e) {
      s.toast(toError(e).message, "error");
    }
  },
  saveTabFile: async (tabId, saveAs) => {
    const s = get();
    const tab = s.tabs.find((t) => t.id === tabId);
    if (!isQueryTab(tab)) return;
    let path = saveAs ? null : (tab.file_path ?? null);
    const fresh = !path;
    if (!path) {
      const { save } = await import("@tauri-apps/plugin-dialog");
      const name = tab.file_path ? baseName(tab.file_path) : `${tab.title.replace(/[\\/:*?"<>|]/g, "_") || "query"}.sql`;
      const picked = await save({ defaultPath: name, filters: [{ name: "SQL", extensions: ["sql"] }] });
      if (!picked) return;
      path = picked;
    }
    const text = editorBridge.get(tabId)?.state.doc.toString() ?? tab.sql;
    try {
      // Saving over the file the tab was read from: refuse if another program changed it.
      const expected = !fresh && tab.file_mtime !== undefined ? tab.file_mtime : null;
      const mtime = await api.writeSqlFile(path, text, expected);
      get().updateTab(tabId, { file_path: path, file_text: text, file_mtime: mtime, dirty: false, title: fresh ? baseName(path) : tab.title });
      rememberFile(store, path);
      get().toast(`Saved ${baseName(path)}`, "success");
    } catch (e) {
      const err = toError(e);
      if (err.kind === "conflict") {
        get().askConfirm({
          title: `${baseName(path)} changed on disk`,
          reasons: [err.message, "Overwrite it with the editor's text, or cancel and reload it from the tab's menu."],
          confirmLabel: "Overwrite",
          onConfirm: () => {
            void api.writeSqlFile(path!, text, null).then(
              (mtime) => {
                get().updateTab(tabId, { file_text: text, file_mtime: mtime, dirty: false });
                get().toast(`Saved ${baseName(path!)}`, "success");
              },
              (e2) => get().toast(toError(e2).message, "error"),
            );
          },
        });
      } else get().toast(err.message, "error");
    }
  },
  checkFiles: async () => {
    const tabs = get().tabs.filter((t) => t.file_path && t.file_mtime !== undefined);
    if (!tabs.length || !isTauri()) return;
    const times = await api.sqlFileMtimes(tabs.map((t) => t.file_path!)).catch(() => null);
    if (!times) return;
    for (const [i, t] of tabs.entries()) {
      const m = times[i];
      if (m === null || m === t.file_mtime) continue;
      const cur = get().tabs.find((x) => x.id === t.id);
      if (!cur) continue;
      const f = await api.readSqlFile(t.file_path!).catch(() => null);
      if (!f) continue;
      if (f.text === cur.file_text) {
        get().updateTab(t.id, { file_mtime: f.mtime_ms });
      } else if (!cur.dirty) {
        get().updateTab(t.id, { sql: f.text, file_text: f.text, file_mtime: f.mtime_ms, dirty: false });
        get().toast(`Reloaded ${baseName(f.path)} (changed on disk)`, "info");
      } else {
        // Ask once per outside change.
        get().updateTab(t.id, { file_mtime: f.mtime_ms, file_text: f.text });
        get().askConfirm({
          title: `${baseName(f.path)} changed on disk`,
          reasons: ["Another program changed the file, and this tab has unsaved edits."],
          confirmLabel: "Reload from disk",
          onConfirm: () => get().updateTab(t.id, { sql: f.text, dirty: false }),
        });
      }
    }
  },
  newTab: (init) => {
    const s = get();
    const active = s.tabs.find((t) => t.id === s.activeTabId);
    const tab: Tab = {
      id: uid(),
      title: init?.title ?? nextTabTitle(s.tabs),
      sql: init?.sql ?? "",
      connection_id:
        init?.connection_id !== undefined
          ? init.connection_id
          : (active?.connection_id ?? s.connections[0]?.id ?? null),
      saved_query_id: init?.saved_query_id ?? null,
      output_ref: init?.output_ref ?? null,
      ddl: init?.ddl ?? null,
      lineage: init?.lineage ?? null,
      job_id: init?.job_id ?? null,
      plan: init?.plan ?? null,
      compare: init?.compare ?? null,
      er: init?.er ?? null,
      dashboard_id: init?.dashboard_id ?? null,
    };
    const tabs = [...s.tabs, tab];
    set({ tabs, activeTabId: tab.id });
    persistTabs(tabs);
    if (isTauri()) api.setSetting("active_tab", tab.id).catch(() => {});
    return tab.id;
  },
  closeTab: (id) => {
    flushDraft(id);
    const s = get();
    const idx = s.tabs.findIndex((t) => t.id === id);
    if (idx === -1) return;
    if (s.txOpen[id] && !window.confirm("This tab has an open transaction. Close it and roll back the uncommitted changes?")) return;
    if (isTauri() && isQueryTab(s.tabs[idx])) api.closeTab(id).catch(() => {});
    // Dashboards: their charts' sessions and results.
    const dash = s.tabs[idx].dashboard_id;
    if (isTauri() && dash) {
      for (const c of s.dashboards.find((d) => d.id === dash)?.charts ?? []) api.closeTab(`dash:${dash}:${c.id}`).catch(() => {});
    }
    let tabs = s.tabs.filter((t) => t.id !== id);
    let activeTabId = s.activeTabId;
    if (tabs.length === 0) {
      tabs = [
        { id: uid(), title: "Query 1", sql: "", connection_id: s.tabs[idx].connection_id ?? null },
      ];
    }
    if (activeTabId === id) activeTabId = tabs[Math.min(idx, tabs.length - 1)].id;
    const runs = { ...s.runs };
    delete runs[id];
    const txOpen = { ...s.txOpen };
    delete txOpen[id];
    const params = { ...s.params };
    delete params[id];
    // Remembered to reopen (not empty scratch tabs); the editor text and
    // parameters come back, the result doesn't (its session was closed).
    const closed = s.tabs[idx];
    const keep = !!(closed.sql.trim() || closed.notebook_id || closed.output_ref || closed.ddl || closed.job_id || closed.saved_query_id);
    const closedTabs = keep ? [...s.closedTabs, { tab: closed, index: idx, params: s.params[id] }].slice(-20) : s.closedTabs;
    set({ tabs, activeTabId, runs, params, closedTabs, txOpen });
    persistParams(params);
    persistTabs(tabs);
  },
  reopenClosedTab: () => {
    flushDrafts();
    const s = get();
    const last = s.closedTabs[s.closedTabs.length - 1];
    if (!last) return false;
    // A new id: the old one's session and outputs are gone.
    const tab: Tab = { ...last.tab, id: uid() };
    const savedParams = last.params;
    const tabs = s.tabs.slice();
    // Replace the lone empty tab left after closing the last one.
    const lone = tabs.length === 1 && !tabs[0].sql.trim() && isQueryTab(tabs[0]);
    if (lone) tabs.splice(0, 1);
    tabs.splice(Math.min(last.index, tabs.length), 0, tab);
    const params = savedParams ? { ...s.params, [tab.id]: savedParams } : s.params;
    set({ tabs, activeTabId: tab.id, closedTabs: s.closedTabs.slice(0, -1), params });
    persistParams(params);
    persistTabs(tabs);
    if (isTauri()) api.setSetting("active_tab", tab.id).catch(() => {});
    return true;
  },
  setActiveTab: (id) => {
    set({ activeTabId: id });
    if (isTauri()) api.setSetting("active_tab", id).catch(() => {});
  },
  updateTab: (id, patch) => {
    // Text set from outside the editor (a reload, a saved query) follows what was typed.
    if (patch.sql !== undefined) flushDraft(id);
    const tabs = get().tabs.map((t) => (t.id === id ? { ...t, ...patch } : t));
    set({ tabs });
    persistTabs(tabs);
  },
  moveTab: (from, to) => {
    const tabs = get().tabs.slice();
    const [t] = tabs.splice(from, 1);
    tabs.splice(to, 0, t);
    set({ tabs });
    persistTabs(tabs);
  },
});
