// "Load into…" loads of a result into a table, followed outside their
// dialog: the dialog may close while one runs ("Continue in background"),
// and the activity bar shows it, its progress and how it ended.
import { create } from "zustand";
import { api, onLoadProgress } from "./lib/api";
import { useActivity } from "./activityStore";
import { useStore } from "./store";
import { formatCount } from "./lib/util";

export interface BackgroundLoad {
  id: string;
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
}

interface LoadsState {
  items: BackgroundLoad[];
  /** The load shown in its dialog: the bar leaves it out meanwhile. */
  inDialog: string | null;
  setInDialog: (id: string | null) => void;
  /** A load started (in the dialog). */
  begin: (l: { id: string; table: string; target: string; total: number }) => void;
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
    begin: ({ id, table, target, total }) => {
      listening ??= onLoadProgress((e) => {
        if (item(e.load_id)?.status === "running") patch(e.load_id, { done: e.done, total: e.total || item(e.load_id)!.total });
      }).catch(() => {});
      if (item(id)) return patch(id, { status: "running", stopping: false });
      set((s) => ({ items: [...s.items, { id, table, target, done: 0, total, started: Date.now(), status: "running" }] }));
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
      if (l && get().inDialog !== id) useStore.getState().toast(`Load into ${l.table} stopped`, "info");
    },
    cancel: (id) => {
      patch(id, { stopping: true });
      void api.cancelLoad(id).catch(() => {});
    },
    dismiss: (id) => set((s) => ({ items: s.items.filter((x) => x.id !== id), inDialog: s.inDialog === id ? null : s.inDialog })),
  };
});
