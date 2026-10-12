import { create } from "zustand";
import { api, onExportProgress, toError, type ExportPhase } from "./lib/api";
import type { ExportOptions } from "./lib/types";
import { useStore } from "./store";
import { uid } from "./lib/util";

/**
 * Full exports ("All rows of the query") run in the background: the query
 * runs again on its own session and every row goes straight to the file, so
 * the app stays usable while a slow query exports. Shown in the exports tray.
 */
export type ExportStatus = "running" | "done" | "failed";

export interface BackgroundExport {
  id: string;
  /** File name (without the folder). */
  fileName: string;
  path: string;
  /** Where it came from (tab title). */
  source: string;
  phase: ExportPhase;
  rows: number;
  bytes: number;
  started: number;
  finished?: number;
  status: ExportStatus;
  error?: string;
  /** Stop was pressed; waiting for the backend to stop. */
  stopping?: boolean;
}

interface ExportsState {
  items: BackgroundExport[];
  /** Export shown in the export dialog (the tray leaves it out meanwhile). */
  inDialog: string | null;
  setInDialog: (id: string | null) => void;
  /** Start a full export; resolves with its id once it is running. */
  start: (a: { resultId: string; options: ExportOptions; path: string; source: string }) => Promise<string>;
  cancel: (id: string) => void;
  dismiss: (id: string) => void;
  reveal: (id: string) => void;
}

/** Finished exports leave the tray on their own after this long. */
export const DONE_VISIBLE_MS = 30_000;

let listening: Promise<unknown> | null = null;

export const useExports = create<ExportsState>((set, get) => {
  const patch = (id: string, p: Partial<BackgroundExport>) =>
    set((s) => ({ items: s.items.map((x) => (x.id === id ? { ...x, ...p } : x)) }));

  const listen = () =>
    (listening ??= onExportProgress((e) => {
      const cur = get().items.find((x) => x.id === e.export_id);
      if (cur?.status === "running") patch(e.export_id, { phase: e.phase, rows: e.rows, bytes: e.bytes });
    }).catch(() => {}));

  return {
    items: [],
    inDialog: null,
    setInDialog: (inDialog) => set({ inDialog }),

    start: async ({ resultId, options, path, source }) => {
      await listen();
      const id = uid();
      const item: BackgroundExport = {
        id,
        fileName: path.split(/[\\/]/).pop() || path,
        path,
        source,
        phase: "connecting",
        rows: 0,
        bytes: 0,
        started: Date.now(),
        status: "running",
      };
      set((s) => ({ items: [...s.items, item] }));
      void api
        .exportFullResult(resultId, options, path, id)
        .then((rows) => {
          patch(id, { status: "done", rows, finished: Date.now(), stopping: false });
          setTimeout(() => {
            if (get().items.find((x) => x.id === id)?.status === "done") get().dismiss(id);
          }, DONE_VISIBLE_MS);
        })
        .catch((e) => {
          const err = toError(e);
          if (err.kind === "cancelled") {
            get().dismiss(id);
            useStore.getState().toast(`Export of ${item.fileName} stopped`, "info");
          } else patch(id, { status: "failed", error: err.message, finished: Date.now(), stopping: false });
        });
      return id;
    },

    cancel: (id) => {
      patch(id, { stopping: true });
      void api.cancelExport(id).catch(() => {});
    },

    dismiss: (id) => set((s) => ({ items: s.items.filter((x) => x.id !== id) })),

    reveal: (id) => {
      api.revealExportFile(id).catch((e) => useStore.getState().toast(toError(e).message, "error"));
    },
  };
});

/** What a running export is doing, for people. */
export function phaseLabel(p: ExportPhase): string {
  switch (p) {
    case "connecting":
      return "Connecting…";
    case "running":
      return "Running the query…";
    case "downloading":
      return "Downloading the result…";
    case "writing":
      return "Writing rows…";
    case "finishing":
      return "Finishing the file…";
  }
}
