// Loads of rows into a table, followed outside their dialog: "Load into…"
// and test data's "Generate and load" (its rows made first, then loaded).
// The dialog may close while one runs ("Continue in background"), and the
// activity bar shows it, its progress and how it ended.
import { create } from "zustand";
import { api, onLoadProgress } from "./lib/api";
import { useActivity } from "./activityStore";
import { useStore } from "./store";
import { formatCount } from "./lib/util";
import { registerTabUser } from "./tabUsers";

export interface BackgroundLoad {
  id: string;
  /** What it is, when not "Load into <table>" ("Test data into orders"). */
  label?: string;
  /** Before the load: what it does ("Making 100 rows…", "OpenAI is writing 300 rows…"). */
  phase?: string;
  /** The AI request making its rows, which Stop cancels before the load starts. */
  aiRun?: string;
  /** The table and the connection it goes into. */
  table: string;
  target: string;
  /** Rows written so far, out of `total` (0: not known, rows read as the query runs). */
  done: number;
  total: number;
  started: number;
  finished?: number;
  status: "running" | "done" | "failed";
  error?: string;
  /** Stop was pressed; waiting for the load to stop. */
  stopping?: boolean;
  /** The tab whose session runs its query again ("every row"): closing it stops the load. */
  tabId?: string;
  /** Why it stopped, when not Stop ("its tab was closed"). */
  stopReason?: string;
}

interface LoadsState {
  items: BackgroundLoad[];
  /** The load shown in its dialog: the bar leaves it out meanwhile. */
  inDialog: string | null;
  setInDialog: (id: string | null) => void;
  /** A load started (in the dialog). */
  begin: (l: { id: string; table: string; target: string; total: number; tabId?: string; label?: string; phase?: string; aiRun?: string }) => void;
  /** It wrote `rows`: kept for the bar unless its dialog shows it, or it was hidden (a toast). */
  finish: (id: string, rows: number) => void;
  /** It failed: kept for the bar unless its dialog shows the error. */
  fail: (id: string, message: string) => void;
  /** It was stopped. */
  stopped: (id: string) => void;
  cancel: (id: string) => void;
  dismiss: (id: string) => void;
}

let listening: Promise<unknown> | null = null;

export const useLoads = create<LoadsState>((set, get) => {
  const patch = (id: string, p: Partial<BackgroundLoad>) => set((s) => ({ items: s.items.map((x) => (x.id === id ? { ...x, ...p } : x)) }));
  const item = (id: string) => get().items.find((x) => x.id === id);
  return {
    items: [],
    inDialog: null,
    setInDialog: (inDialog) => set({ inDialog }),
    begin: ({ id, table, target, total, tabId, label, phase, aiRun }) => {
      listening ??= onLoadProgress((e) => {
        if (item(e.load_id)?.status === "running") patch(e.load_id, { done: e.done, total: e.total || item(e.load_id)!.total });
      }).catch(() => {});
      // Again (rows made, now loading; or after a production prompt): the new phase, the same item.
      if (item(id)) return patch(id, { status: "running", stopping: false, total, phase, aiRun });
      set((s) => ({ items: [...s.items, { id, table, target, done: 0, total, started: Date.now(), status: "running", tabId, label, phase, aiRun }] }));
    },
    finish: (id, rows) => {
      const l = item(id);
      if (!l) return;
      const hidden = useActivity.getState().ended(`load:${id}`);
      if (get().inDialog === id || hidden) {
        get().dismiss(id);
        if (hidden) useStore.getState().toast(`Loaded ${formatCount(rows)} rows into ${l.table}`, "success");
        return;
      }
      patch(id, { status: "done", done: rows, finished: Date.now(), stopping: false });
    },
    fail: (id, message) => {
      useActivity.getState().ended(`load:${id}`);
      if (get().inDialog === id) return get().dismiss(id);
      patch(id, { status: "failed", error: message, finished: Date.now(), stopping: false });
    },
    stopped: (id) => {
      const l = item(id);
      useActivity.getState().ended(`load:${id}`);
      get().dismiss(id);
      if (l && get().inDialog !== id) useStore.getState().toast(`${l.label ?? `Load into ${l.table}`} stopped${l.stopReason ? `: ${l.stopReason}` : ""}`, "info");
    },
    cancel: (id) => {
      const l = item(id);
      patch(id, { stopping: true });
      // Still making its rows with the AI: that request stops; else the load.
      if (l?.phase && l.aiRun) void api.aiCancel(l.aiRun).catch(() => {});
      else void api.cancelLoad(id).catch(() => {});
    },
    dismiss: (id) => set((s) => ({ items: s.items.filter((x) => x.id !== id), inDialog: s.inDialog === id ? null : s.inDialog })),
  };
});

registerTabUser({
  uses: (tabId) =>
    useLoads
      .getState()
      .items.filter((l) => l.status === "running" && l.tabId === tabId)
      .map((l) => `the load into ${l.table}`),
  closing: (tabId) =>
    useLoads.setState((s) => ({ items: s.items.map((l) => (l.status === "running" && l.tabId === tabId ? { ...l, stopReason: "its tab was closed" } : l)) })),
});
