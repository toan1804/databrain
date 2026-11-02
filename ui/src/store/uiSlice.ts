import type { StateCreator } from "zustand";
import { DEFAULT_APPEARANCE, applyAppearance, parseAppearance, type Appearance } from "../lib/appearance";
import { api, isTauri, type SshHostKeyPrompt } from "../lib/api";
import { DEFAULT_SLOW_QUERY_SECONDS } from "../queryTips";
import { uid } from "../lib/util";
import { parseSnippets, type Snippet } from "../lib/snippets";
import { parseViews, rememberQuickFilter, type QuickFilterEntry, type SavedView } from "../lib/gridViews";
import { applyTheme, persistQuickFilters } from "./helpers";
import type { ConfirmState, SidebarPanel, State, Theme, Toast } from "./types";

let appearanceSave: ReturnType<typeof setTimeout> | undefined;

export interface UiSlice {
  settingsOpen: boolean;
  theme: Theme;
  /** Background, fonts and code font size (Settings → Appearance). */
  appearance: Appearance;
  setAppearance: (patch: Partial<Appearance>) => void;
  rowLimit: number;
  /** Tips are computed for statements running at least this long (0 = off). */
  slowQuerySeconds: number;
  setSlowQuerySeconds: (s: number) => void;
  /** Inline AI suggestions while typing (off by default: it sends the SQL around the cursor to the AI provider). */
  inlineAiCompletion: boolean;
  setInlineAiCompletion: (on: boolean) => void;
  /** The user's SQL snippets (built-in ones are added by the editor). */
  sqlSnippets: Snippet[];
  setSqlSnippets: (s: Snippet[]) => void;
  /** Saved result grid views (setting `grid_views`). */
  gridViews: SavedView[];
  saveGridView: (v: SavedView) => void;
  deleteGridView: (id: string) => void;
  /** Quick filters by table (`tableKey`), restored when the table's rows are shown again. */
  quickFilters: Record<string, QuickFilterEntry>;
  setQuickFilter: (key: string, q: string) => void;
  sidebarPanel: SidebarPanel;
  toasts: Toast[];
  confirm: ConfirmState | null;
  paletteOpen: boolean;
  /** Show the Oracle Instant Client install prompt. */
  oracleClientPrompt: boolean;
  /** An SSH server whose key the user is asked to check and trust. */
  sshHostKey: SshHostKeyPrompt | null;
  toast: (message: string, kind?: Toast["kind"]) => void;
  dismissToast: (id: string) => void;
  setTheme: (t: Theme) => void;
  setRowLimit: (n: number) => void;
  setSidebarPanel: (p: SidebarPanel) => void;
  setSettingsOpen: (open: boolean) => void;
  setPaletteOpen: (open: boolean) => void;
  askConfirm: (c: ConfirmState | null) => void;
}

export const createUiSlice: StateCreator<State, [], [], UiSlice> = (set, get) => ({
  settingsOpen: false,
  theme: "dark",
  appearance: DEFAULT_APPEARANCE,
  rowLimit: 1000,
  slowQuerySeconds: DEFAULT_SLOW_QUERY_SECONDS,
  inlineAiCompletion: false,
  sqlSnippets: [],
  gridViews: [],
  quickFilters: {},
  sidebarPanel: "connections",
  oracleClientPrompt: false,
  sshHostKey: null,
  toasts: [],
  confirm: null,
  paletteOpen: false,
  toast: (message, kind = "info") => {
    const id = uid();
    set((s) => ({ toasts: [...s.toasts, { id, kind, message }] }));
    setTimeout(() => get().dismissToast(id), kind === "error" ? 8000 : 3500);
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),

  setTheme: (theme) => {
    applyTheme(theme, get().appearance);
    set({ theme });
    if (isTauri()) api.setSetting("theme", theme).catch(() => {});
  },
  setAppearance: (patch) => {
    const appearance = parseAppearance({ ...get().appearance, ...patch });
    // Before the store changes, like the theme: the grid reads the variables while rendering.
    applyAppearance(appearance, get().theme);
    set({ appearance });
    // The colour picker and size slider change it continuously: save once they settle.
    clearTimeout(appearanceSave);
    if (isTauri()) appearanceSave = setTimeout(() => api.setSetting("appearance", get().appearance).catch(() => {}), 300);
  },
  setRowLimit: (rowLimit) => {
    set({ rowLimit });
    if (isTauri()) api.setSetting("row_limit", rowLimit).catch(() => {});
  },
  setSlowQuerySeconds: (n) => {
    const slowQuerySeconds = Number.isFinite(n) ? Math.max(0, Math.round(n)) : DEFAULT_SLOW_QUERY_SECONDS;
    set({ slowQuerySeconds });
    if (isTauri()) api.setSetting("slow_query_seconds", slowQuerySeconds).catch(() => {});
  },
  setInlineAiCompletion: (inlineAiCompletion) => {
    set({ inlineAiCompletion });
    if (isTauri()) api.setSetting("inline_ai_completion", inlineAiCompletion).catch(() => {});
  },
  setSqlSnippets: (s) => {
    const sqlSnippets = parseSnippets(s);
    set({ sqlSnippets });
    if (isTauri()) api.setSetting("sql_snippets", sqlSnippets).catch(() => {});
  },
  saveGridView: (v) => {
    const gridViews = parseViews([v, ...get().gridViews.filter((x) => x.id !== v.id)]);
    set({ gridViews });
    if (isTauri()) api.setSetting("grid_views", gridViews).catch(() => {});
  },
  deleteGridView: (id) => {
    const gridViews = get().gridViews.filter((x) => x.id !== id);
    set({ gridViews });
    if (isTauri()) api.setSetting("grid_views", gridViews).catch(() => {});
  },
  setQuickFilter: (key, q) => {
    const cur = get().quickFilters[key];
    if ((cur?.q ?? "") === q.trim()) return;
    const quickFilters = rememberQuickFilter(get().quickFilters, key, q.trim());
    set({ quickFilters });
    persistQuickFilters(quickFilters);
  },
  setSidebarPanel: (sidebarPanel) => set({ sidebarPanel }),
  setSettingsOpen: (settingsOpen) => set({ settingsOpen }),
  setPaletteOpen: (paletteOpen) => set({ paletteOpen }),
  askConfirm: (confirm) => set({ confirm }),
});
