import type { StateCreator } from "zustand";
import { parseAppearance } from "../lib/appearance";
import { setSourceTraits } from "../lib/sourceTraits";
import { type SshHostKeyPrompt, api, isTauri, onAuthEvent, onJobEvent, onOracleAgent, toError } from "../lib/api";
import { DEFAULT_SLOW_QUERY_SECONDS } from "../queryTips";
import { uid } from "../lib/util";
import type { Dashboard } from "../lib/charts";
import { parseSnippets } from "../lib/snippets";
import { parseInlineAiProvider } from "../lib/inlineAi";
import { parseQuickFilters, parseViews } from "../lib/gridViews";
import { applyTheme, nextSignIn, parseParams, parseSchemaFilter } from "./helpers";
import type { State, Tab } from "./types";

let storeInitStarted = false;

export interface AppSlice {
  ready: boolean;
  backendAvailable: boolean;
  init: () => Promise<void>;
}

export const createAppSlice: StateCreator<State, [], [], AppSlice> = (set, get) => ({
  ready: false,
  backendAvailable: false,
  init: async () => {
    // Once per app: React StrictMode runs effects twice in dev, which would
    // register every backend event listener twice.
    if (storeInitStarted) return;
    storeInitStarted = true;
    if (!isTauri()) {
      const id = uid();
      set({
        ready: true,
        backendAvailable: false,
        tabs: [{ id, title: "Query 1", sql: "select 1;" }],
        activeTabId: id,
      });
      return;
    }
    try {
      const [connectors, connections, tabs, settings, savedQueries] = await Promise.all([
        api.listConnectors().then((c) => (setSourceTraits(c), c)),
        api.listConnections(),
        api.loadTabs(),
        api.getSettings(),
        api.listSavedQueries(null),
      ]);
      const theme = settings.theme === "light" ? "light" : "dark";
      const appearance = parseAppearance(settings.appearance);
      applyTheme(theme, appearance);
      const rowLimit = typeof settings.row_limit === "number" ? settings.row_limit : 1000;
      const slowQuerySeconds = typeof settings.slow_query_seconds === "number" ? settings.slow_query_seconds : DEFAULT_SLOW_QUERY_SECONDS;
      let restored: Tab[] = tabs;
      if (restored.length === 0) {
        restored = [
          {
            id: uid(),
            title: "Query 1",
            sql: "",
            connection_id: connections[0]?.id ?? null,
          },
        ];
      }
      const active =
        typeof settings.active_tab === "string" && restored.some((t) => t.id === settings.active_tab)
          ? (settings.active_tab as string)
          : restored[0].id;
      set({
        ready: true,
        backendAvailable: true,
        connectors,
        connections,
        tabs: restored,
        activeTabId: active,
        theme,
        appearance,
        rowLimit,
        slowQuerySeconds,
        inlineAiCompletion: settings.inline_ai_completion === true,
        inlineAiProvider: parseInlineAiProvider(settings.inline_ai_provider),
        sqlSnippets: parseSnippets(settings.sql_snippets),
        gridViews: parseViews(settings.grid_views),
        quickFilters: parseQuickFilters(settings.grid_quick_filters),
        savedQueries,
        schemaFilter: parseSchemaFilter(settings.explorer_schemas),
        params: parseParams(settings.editor_params, restored.map((t) => t.id)),
        recentFiles: Array.isArray(settings.recent_sql_files) ? settings.recent_sql_files.filter((p): p is string => typeof p === "string") : [],
        dashboards: Array.isArray(settings.dashboards) ? (settings.dashboards as Dashboard[]).filter((d) => d && typeof d.id === "string" && Array.isArray(d.charts)) : [],
      });
      // File tabs: what is on disk now (edits not saved last time stay, marked unsaved).
      for (const t of restored.filter((x) => x.file_path)) {
        void api.readSqlFile(t.file_path!).then(
          (f) => get().updateTab(t.id, { file_text: f.text, file_mtime: f.mtime_ms, dirty: f.text !== t.sql }),
          () => get().updateTab(t.id, { dirty: true }),
        );
      }
      await onJobEvent((e) => get().handleJobEvent(e));
      window.addEventListener("db:oracle-client-missing", () => set({ oracleClientPrompt: true }));
      window.addEventListener("db:ssh-host-key", (e) => set({ sshHostKey: (e as CustomEvent<SshHostKeyPrompt>).detail }));
      // Thin Oracle driver: downloaded once, on the first Oracle connection.
      let agentToast = false;
      void onOracleAgent((e) => {
        if (e.type === "progress" && !agentToast) {
          agentToast = true;
          get().toast("Downloading the Oracle driver (about 5 MB, once)…", "info");
        } else if (e.type === "finished" && agentToast) {
          agentToast = false;
          if (e.ok) get().toast("Oracle driver ready", "success"); // failures surface as the connection error
        }
      }).catch(() => {});
      void import("../components/OracleClient").then((m) => m.checkOracleClientAtStartup());
      await onAuthEvent((event) => {
        const was = get().signIn;
        const next = nextSignIn(was, event);
        set({ signIn: next });
        if (event.type === "finished" && was?.implicit) {
          // The connection the sign-in was for is connected (or failed) now.
          void get().refreshConnections().catch(() => {});
        }
      });
      void get().refreshFolders();
      void get().refreshNotebooks();
      void get()
        .refreshOutputs()
        .then(() => get().restoreTabOutputs());
    } catch (e) {
      set({ ready: true });
      get().toast(`Failed to start: ${toError(e).message}`, "error");
    }
  },
});
