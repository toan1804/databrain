import type { StateCreator } from "zustand";
import { flushDraft } from "../drafts";
import { api, isTauri, toError } from "../lib/api";
import type { Folder, FolderKind, NotebookSummary, SavedQuery } from "../lib/types";
import { uid } from "../lib/util";
import type { Dashboard } from "../lib/charts";
import { persistTabs } from "./helpers";
import { isQueryTab, type State, type Tab } from "./types";

export interface LibrarySlice {
  folders: Record<FolderKind, Folder[]>;
  notebooks: NotebookSummary[];
  /** Dashboards of pinned charts (`dashboards` setting). */
  dashboards: Dashboard[];
  saveDashboards: (d: Dashboard[]) => void;
  /** Open a dashboard in a tab (focuses it when open). */
  openDashboard: (id: string) => void;
  savedQueries: SavedQuery[];
  saveQueryDialog: { open: boolean; tabId?: string };
  refreshFolders: (kind?: FolderKind) => Promise<void>;
  refreshNotebooks: () => Promise<void>;
  openNotebook: (id: string, name: string) => void;
  /** A new notebook: no connection unless given (the user picks one), at the root or in `folderId`. */
  newNotebook: (connectionId?: string | null, folderId?: string | null) => Promise<void>;
  refreshSavedQueries: () => Promise<void>;
  openSavedQuery: (q: SavedQuery) => void;
  saveTabQuery: (tabId: string) => Promise<void>;
  setSaveQueryDialog: (s: { open: boolean; tabId?: string }) => void;
}

export const createLibrarySlice: StateCreator<State, [], [], LibrarySlice> = (set, get) => ({
  folders: { connections: [], queries: [], notebooks: [] },
  notebooks: [],
  dashboards: [],
  saveDashboards: (dashboards) => {
    set({ dashboards });
    if (isTauri()) void api.setSetting("dashboards", dashboards).catch((e) => get().toast(toError(e).message, "error"));
    // Tabs of dashboards that are gone close.
    for (const t of get().tabs) if (t.dashboard_id && !dashboards.some((d) => d.id === t.dashboard_id)) get().closeTab(t.id);
  },
  openDashboard: (id) => {
    const s = get();
    const open = s.tabs.find((t) => t.dashboard_id === id);
    if (open) return s.setActiveTab(open.id);
    const d = s.dashboards.find((x) => x.id === id);
    if (d) s.newTab({ title: d.name, sql: "", dashboard_id: id, connection_id: null });
  },
  savedQueries: [],
  saveQueryDialog: { open: false },
  refreshFolders: async (kind) => {
    const kinds: FolderKind[] = kind ? [kind] : ["connections", "queries", "notebooks"];
    const lists = await Promise.all(kinds.map((k) => api.listFolders(k)));
    set((s) => {
      const folders = { ...s.folders };
      kinds.forEach((k, i) => (folders[k] = lists[i]));
      return { folders };
    });
  },
  refreshNotebooks: async () => {
    set({ notebooks: await api.listNotebooks() });
  },
  openNotebook: (id, name) => {
    const s = get();
    const existing = s.tabs.find((t) => t.notebook_id === id);
    if (existing) return s.setActiveTab(existing.id);
    const tab: Tab = { id: uid(), title: name, sql: "", notebook_id: id, connection_id: null };
    const tabs = [...s.tabs, tab];
    set({ tabs, activeTabId: tab.id });
    persistTabs(tabs);
  },
  newNotebook: async (connectionId, folderId) => {
    const s = get();
    const conn = connectionId ?? null;
    let n = 1;
    while (s.notebooks.some((x) => x.name === `Notebook ${n}`)) n++;
    try {
      const nb = await api.saveNotebook({
        id: "",
        name: `Notebook ${n}`,
        connection_id: conn,
        folder_id: folderId ?? null,
        cells: [
          { id: uid(), kind: "markdown", source: "# Notebook\nDescribe the analysis here." },
          { id: uid(), kind: "sql", source: "" },
        ],
        created_at: 0,
        updated_at: 0,
      });
      await get().refreshNotebooks();
      get().openNotebook(nb.id, nb.name);
    } catch (e) {
      get().toast(toError(e).message, "error");
    }
  },
  refreshSavedQueries: async () => {
    const savedQueries = await api.listSavedQueries(null);
    set({ savedQueries });
  },
  openSavedQuery: (q) => {
    const s = get();
    const existing = s.tabs.find((t) => t.saved_query_id === q.id);
    if (existing) {
      s.setActiveTab(existing.id);
      return;
    }
    s.newTab({
      title: q.name,
      sql: q.sql,
      saved_query_id: q.id,
      connection_id: q.connection_id ?? s.tabs.find((t) => t.id === s.activeTabId)?.connection_id,
    });
  },
  saveTabQuery: async (tabId) => {
    flushDraft(tabId);
    const s = get();
    const tab = s.tabs.find((t) => t.id === tabId);
    if (!isQueryTab(tab)) return;
    const existing = tab.saved_query_id
      ? s.savedQueries.find((q) => q.id === tab.saved_query_id)
      : undefined;
    if (!existing) {
      set({ saveQueryDialog: { open: true, tabId } });
      return;
    }
    try {
      const saved = await api.saveQuery({
        ...existing,
        sql: tab.sql,
        connection_id: tab.connection_id ?? null,
      });
      s.updateTab(tabId, { dirty: false, title: saved.name });
      await get().refreshSavedQueries();
      get().toast(`Saved "${saved.name}"`, "success");
    } catch (e) {
      get().toast(toError(e).message, "error");
    }
  },
  setSaveQueryDialog: (saveQueryDialog) => set({ saveQueryDialog }),
});
