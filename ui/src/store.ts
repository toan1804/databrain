import { create } from "zustand";
import { revealKeys, splitSchema, treeKey } from "./lib/catalog";
import { api, isTauri, onAuthEvent, onJobEvent, onOracleAgent, toError } from "./lib/api";
import { DEFAULT_SLOW_QUERY_SECONDS, computeRunTips, slowStatements, type RunTips } from "./queryTips";
import type {
  AuthEvent,
  Folder,
  FolderKind,
  NotebookSummary,
  OutputInfo,
  ColumnInfo,
  ConnectionView,
  ConnectorInfo,
  DbObject,
  EngineError,
  JobEvent,
  PlannedStatement,
  ResultInfo,
  RunStatus,
  SavedQuery,
  SchemaInfo,
  TabState,
} from "./lib/types";
import { errorRange, uid } from "./lib/util";

export interface Tab extends TabState {
  /** Unsaved changes relative to the linked saved query. */
  dirty?: boolean;
}

export type StatementStatus = "pending" | "running" | "done" | "error" | "cancelled";

export interface StatementRun {
  plan: PlannedStatement;
  status: StatementStatus;
  result?: ResultInfo;
  output?: OutputInfo;
  rowsAffected?: number | null;
  durationMs?: number;
  /** When the statement started running (ms since epoch). */
  startedAt?: number;
  progressRows?: number;
  notices: string[];
  error?: EngineError;
}

export interface TabRun {
  jobId: string | null;
  running: boolean;
  startedAt: number;
  finishedStatus?: RunStatus;
  durationMs?: number;
  statements: StatementRun[];
  /** Index into `statements` of the result shown in the grid. */
  activeIndex: number | null;
  errorRange?: { from: number; to: number };
  /** Suggestions to improve the query, computed after it ran. */
  tips?: RunTips;
  connectionId?: string;
}

export interface Toast {
  id: string;
  kind: "info" | "success" | "error";
  message: string;
}

export interface ConfirmState {
  title: string;
  reasons: string[];
  confirmLabel: string;
  onConfirm: () => void;
  onCancel?: () => void;
}

export type SidebarPanel = "connections" | "saved" | "notebooks" | "outputs" | "history";
export type Theme = "dark" | "light";

interface RunInput {
  doc: string;
  selFrom: number;
  selTo: number;
  cursor: number;
}

export interface SignInState {
  connectionId: string | null;
  label: string;
  event: AuthEvent | null;
}

interface State {
  ready: boolean;
  folders: Record<FolderKind, Folder[]>;
  /** All known outputs (newest first); refreshed on `outputs_changed`. */
  outputs: OutputInfo[];
  refreshOutputs: () => Promise<void>;
  /** Show each tab's last results from the previous session (active tab first). */
  restoreTabOutputs: () => Promise<void>;
  /** Patch an output in runs + list after rename/pin. */
  applyOutput: (o: OutputInfo) => void;
  /** Forget dropped outputs in runs, output tabs and the list. */
  forgetOutputs: (handles: string[]) => void;
  notebooks: NotebookSummary[];
  signIn: SignInState | null;
  settingsOpen: boolean;
  backendAvailable: boolean;
  connectors: ConnectorInfo[];
  connections: ConnectionView[];
  tabs: Tab[];
  activeTabId: string | null;
  runs: Record<string, TabRun>;
  savedQueries: SavedQuery[];
  historyVersion: number;
  schemas: Record<string, SchemaInfo[]>;
  objects: Record<string, DbObject[]>; // key `${conn}|${schema}`
  columns: Record<string, ColumnInfo[]>; // key `${conn}|${schema}|${name}`
  theme: Theme;
  rowLimit: number;
  /** Tips are computed for statements running at least this long (0 = off). */
  slowQuerySeconds: number;
  setSlowQuerySeconds: (s: number) => void;
  sidebarPanel: SidebarPanel;
  /** Explorer expansion state (keys from `treeKey`). */
  treeOpen: Record<string, boolean>;
  /** Object key to scroll to and highlight after a search reveal. */
  treeFocus: string | null;
  catalogSearch: { query: string; scope: string | null; focusSeq: number };
  toasts: Toast[];
  confirm: ConfirmState | null;
  connectionDialog: { open: boolean; profile?: ConnectionView | null; folderId?: string | null };
  saveQueryDialog: { open: boolean; tabId?: string };
  paletteOpen: boolean;
  /** Show the Oracle Instant Client install prompt. */
  oracleClientPrompt: boolean;

  init: () => Promise<void>;
  toast: (message: string, kind?: Toast["kind"]) => void;
  dismissToast: (id: string) => void;
  setTheme: (t: Theme) => void;
  setRowLimit: (n: number) => void;
  setSidebarPanel: (p: SidebarPanel) => void;
  setTreeOpen: (key: string, open: boolean) => void;
  /** Schemas shown in the explorer per connection (absent = all). Saved in settings. */
  schemaFilter: Record<string, string[]>;
  setSchemaFilter: (connId: string, schemas: string[] | null) => void;
  /** Connection whose "Choose schemas" dialog is open. */
  schemaPicker: string | null;
  /** Expand the explorer down to `obj` and scroll to it. */
  revealObject: (connId: string, obj: DbObject) => void;
  /** Show the catalog search (optionally scoped to one connection) and focus it. */
  openCatalogSearch: (scope?: string | null) => void;
  setCatalogSearch: (patch: Partial<{ query: string; scope: string | null }>) => void;

  refreshConnections: () => Promise<void>;
  openConnectionDialog: (profile?: ConnectionView | null, folderId?: string | null) => void;
  closeConnectionDialog: () => void;
  loadSchemas: (connId: string, force?: boolean) => Promise<SchemaInfo[]>;
  loadObjects: (connId: string, schema: string, force?: boolean) => Promise<DbObject[]>;
  loadColumns: (connId: string, schema: string, name: string) => Promise<ColumnInfo[]>;
  disconnect: (connId: string) => Promise<void>;
  /** Ask, then delete a connection (secrets and AI knowledge too). Resolves true when deleted. */
  deleteConnection: (connId: string) => Promise<boolean>;

  newTab: (init?: Partial<Tab>) => string;
  closeTab: (id: string) => void;
  setActiveTab: (id: string) => void;
  updateTab: (id: string, patch: Partial<Tab>) => void;
  moveTab: (from: number, to: number) => void;

  runTab: (tabId: string, mode: "statement" | "all", input: RunInput) => Promise<void>;
  /** Run SQL under a run key (tab id or notebook cell key `nb:{id}:{cell}`). */
  runSql: (
    runKey: string,
    connectionId: string,
    sql: string,
    base?: number,
    sessionKey?: string,
    outputName?: string | null,
  ) => Promise<boolean>;
  refreshFolders: (kind?: FolderKind) => Promise<void>;
  refreshNotebooks: () => Promise<void>;
  openNotebook: (id: string, name: string) => void;
  newNotebook: (connectionId?: string | null) => Promise<void>;
  signInConnection: (connectionId: string) => Promise<boolean>;
  setSignIn: (s: SignInState | null) => void;
  setSettingsOpen: (open: boolean) => void;
  cancelTab: (tabId: string) => Promise<void>;
  setActiveStatement: (tabId: string, index: number) => void;
  handleJobEvent: (e: JobEvent) => void;

  refreshSavedQueries: () => Promise<void>;
  /** Index / partition / cluster-key tips for the run's slow statements. */
  computeTips: (runKey: string, jobId: string) => Promise<void>;
  openSavedQuery: (q: SavedQuery) => void;
  saveTabQuery: (tabId: string) => Promise<void>;
  setSaveQueryDialog: (s: { open: boolean; tabId?: string }) => void;
  setPaletteOpen: (open: boolean) => void;
  askConfirm: (c: ConfirmState | null) => void;
}

const TAB_SAVE_DELAY = 400;

/** Notice shown on a result restored from the previous session. */
export const RESTORED_NOTICE = "Restored from your last session. Run again for fresh data.";

/**
 * Run state that shows a tab's saved outputs (its last results, saved when
 * the app quit) without re-running anything. `null` when the tab has none.
 */
export function restoredRun(tabId: string, outputs: OutputInfo[]): TabRun | null {
  const mine = outputs.filter((o) => o.tab_id === tabId && o.active && o.state !== "evicted").sort((a, b) => a.statement_index - b.statement_index);
  if (!mine.length) return null;
  return {
    jobId: null,
    running: false,
    startedAt: Math.min(...mine.map((o) => o.created_at)),
    finishedStatus: "success",
    statements: mine.map((o) => ({
      plan: { index: o.statement_index, sql: o.sql, start: 0, end: 0, classification: { kind: "read", missing_where: false, keyword: "" } },
      status: "done",
      result: { id: o.result_id, columns: o.columns, total_rows: o.rows, complete: true, truncated: o.truncated, bytes: o.bytes },
      output: o,
      notices: [RESTORED_NOTICE],
    })),
    activeIndex: mine.length - 1,
  };
}

/**
 * Job events that arrived before the `run_query` response told us the job id.
 * Fast statements (warm session) can finish before the response is delivered;
 * without this buffer their `job_finished` would be dropped and the run would
 * look stuck. Replayed as soon as the run is registered.
 */
let outputsTimer: ReturnType<typeof setTimeout> | undefined;

const earlyEvents = new Map<string, { at: number; events: JobEvent[] }>();
const EARLY_TTL_MS = 60_000;

function bufferEarly(e: JobEvent) {
  const now = Date.now();
  for (const [id, v] of earlyEvents) if (now - v.at > EARLY_TTL_MS) earlyEvents.delete(id);
  const entry = earlyEvents.get(e.job_id) ?? { at: now, events: [] };
  entry.events.push(e);
  earlyEvents.set(e.job_id, entry);
}

/** Test hook. */
export const _earlyEventCount = () => earlyEvents.size;
let tabSaveTimer: ReturnType<typeof setTimeout> | undefined;

function persistTabs(tabs: Tab[]) {
  if (!isTauri()) return;
  clearTimeout(tabSaveTimer);
  tabSaveTimer = setTimeout(() => {
    api
      .saveTabs(
        tabs.map((t) => ({
          id: t.id,
          title: t.title,
          sql: t.sql,
          connection_id: t.connection_id ?? null,
          saved_query_id: t.saved_query_id ?? null,
          notebook_id: t.notebook_id ?? null,
          output_ref: t.output_ref ?? null,
        })),
      )
      .catch(() => {});
  }, TAB_SAVE_DELAY);
}

function nextTabTitle(tabs: Tab[]): string {
  let n = 1;
  const titles = new Set(tabs.map((t) => t.title));
  while (titles.has(`Query ${n}`)) n++;
  return `Query ${n}`;
}

let storeInitStarted = false;

/** `explorer_schemas` setting: { connectionId: [schema ids] }. */
export function parseSchemaFilter(v: unknown): Record<string, string[]> {
  if (!v || typeof v !== "object") return {};
  const out: Record<string, string[]> = {};
  for (const [k, list] of Object.entries(v as Record<string, unknown>)) {
    if (Array.isArray(list)) {
      const names = list.filter((x): x is string => typeof x === "string");
      if (names.length) out[k] = names;
    }
  }
  return out;
}

export const useStore = create<State>((set, get) => ({
  ready: false,
  folders: { connections: [], queries: [], notebooks: [] },
  outputs: [],
  refreshOutputs: async () => {
    if (!isTauri()) return;
    try {
      const outputs = await api.listOutputs();
      set((s) => {
        // DuckDB explorers list outputs under results.main: drop stale caches.
        const duck = new Set(s.connections.filter((c) => c.config.kind === "duckdb").map((c) => c.id));
        const objects = Object.fromEntries(Object.entries(s.objects).filter(([k]) => !k.endsWith("|results.main")));
        const columns = Object.fromEntries(Object.entries(s.columns).filter(([k]) => !k.includes("|results.main|")));
        const schemas = Object.fromEntries(
          Object.entries(s.schemas).filter(([id, list]) => !(duck.has(id) && outputs.length > 0 && !list.some((x) => x.name === "results.main"))),
        );
        return { outputs, objects, columns, schemas };
      });
      // Reload what the explorer shows open.
      const st = get();
      for (const c of st.connections) {
        if (c.config.kind !== "duckdb") continue;
        if (st.treeOpen[`c|${c.id}`] && !st.schemas[c.id]) st.loadSchemas(c.id).catch(() => {});
        if (st.treeOpen[`s|${c.id}|results.main`] && st.schemas[c.id]?.some((x) => x.name === "results.main")) st.loadObjects(c.id, "results.main").catch(() => {});
      }
    } catch {
      /* backend restarting */
    }
  },
  restoreTabOutputs: async () => {
    const { tabs, activeTabId } = get();
    const order = [...tabs].filter((t) => !t.notebook_id && !t.output_ref).sort((a, b) => Number(b.id === activeTabId) - Number(a.id === activeTabId));
    for (const t of order) {
      const outs = get().outputs.filter((o) => o.tab_id === t.id && o.active);
      if (!outs.length || get().runs[t.id]) continue;
      try {
        // Saved results are read from disk on first use.
        const loaded = await Promise.all(outs.map((o) => (o.state === "on_disk" ? api.loadOutput(o.handle) : Promise.resolve(o))));
        const run = restoredRun(t.id, loaded);
        // The user may have run the tab meanwhile.
        if (run && !get().runs[t.id]) set((s) => ({ runs: { ...s.runs, [t.id]: run } }));
      } catch {
        // Snapshot missing or unreadable: the tab just starts empty.
      }
    }
  },
  forgetOutputs: (handles) =>
    set((s) => {
      const gone = new Set(handles);
      const runs = { ...s.runs };
      for (const [k, r] of Object.entries(runs)) {
        if (!r.statements.some((x) => x.output && gone.has(x.output.handle))) continue;
        runs[k] = {
          ...r,
          statements: r.statements.map((x) =>
            x.output && gone.has(x.output.handle) ? { ...x, output: undefined, result: undefined, notices: [...x.notices, `Output ${x.output.handle} was dropped. Run again to see rows.`] } : x,
          ),
        };
      }
      const tabs = s.tabs.filter((t) => !(t.output_ref && gone.has(t.output_ref)));
      const activeTabId = tabs.some((t) => t.id === s.activeTabId) ? s.activeTabId : (tabs[0]?.id ?? null);
      return { runs, tabs, activeTabId, outputs: s.outputs.filter((o) => !gone.has(o.handle)) };
    }),
  applyOutput: (o) =>
    set((s) => {
      const runs = { ...s.runs };
      for (const [k, r] of Object.entries(runs)) {
        if (r.statements.some((x) => x.output?.handle === o.handle)) {
          runs[k] = { ...r, statements: r.statements.map((x) => (x.output?.handle === o.handle ? { ...x, output: o } : x)) };
        }
      }
      return { runs, outputs: s.outputs.map((x) => (x.handle === o.handle ? o : x)) };
    }),
  notebooks: [],
  signIn: null,
  settingsOpen: false,
  backendAvailable: false,
  connectors: [],
  connections: [],
  tabs: [],
  activeTabId: null,
  runs: {},
  savedQueries: [],
  historyVersion: 0,
  schemas: {},
  objects: {},
  columns: {},
  theme: "dark",
  rowLimit: 1000,
  slowQuerySeconds: DEFAULT_SLOW_QUERY_SECONDS,
  sidebarPanel: "connections",
  treeOpen: {},
  schemaFilter: {},
  schemaPicker: null,
  treeFocus: null,
  catalogSearch: { query: "", scope: null, focusSeq: 0 },
  oracleClientPrompt: false,
  toasts: [],
  confirm: null,
  connectionDialog: { open: false },
  saveQueryDialog: { open: false },
  paletteOpen: false,

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
        api.listConnectors(),
        api.listConnections(),
        api.loadTabs(),
        api.getSettings(),
        api.listSavedQueries(null),
      ]);
      const theme = settings.theme === "light" ? "light" : "dark";
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
        rowLimit,
        slowQuerySeconds,
        savedQueries,
        schemaFilter: parseSchemaFilter(settings.explorer_schemas),
      });
      await onJobEvent((e) => get().handleJobEvent(e));
      window.addEventListener("db:oracle-client-missing", () => set({ oracleClientPrompt: true }));
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
      void import("./components/OracleClient").then((m) => m.checkOracleClientAtStartup());
      await onAuthEvent((event) => {
        const cur = get().signIn;
        if (event.type === "finished") set({ signIn: cur ? { ...cur, event } : null });
        else set({ signIn: { connectionId: cur?.connectionId ?? null, label: cur?.label ?? "Sign in", event } });
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

  toast: (message, kind = "info") => {
    const id = uid();
    set((s) => ({ toasts: [...s.toasts, { id, kind, message }] }));
    setTimeout(() => get().dismissToast(id), kind === "error" ? 8000 : 3500);
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),

  setTheme: (theme) => {
    set({ theme });
    if (isTauri()) api.setSetting("theme", theme).catch(() => {});
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
  setSidebarPanel: (sidebarPanel) => set({ sidebarPanel }),
  setTreeOpen: (key, open) => set((s) => ({ treeOpen: { ...s.treeOpen, [key]: open } })),
  setSchemaFilter: (connId, schemas) => {
    const next = { ...get().schemaFilter };
    if (schemas && schemas.length) next[connId] = schemas;
    else delete next[connId];
    set({ schemaFilter: next });
    void api.setSetting("explorer_schemas", next).catch(() => {});
  },
  revealObject: (connId, obj) => {
    const st = get();
    const kind = st.connections.find((c) => c.id === connId)?.config.kind;
    const catalog = kind ? splitSchema(kind, obj.schema, st.schemas[connId]).catalog : undefined;
    const open = { ...st.treeOpen };
    for (const k of revealKeys(connId, obj, catalog)) open[k] = true;
    set((s) => ({
      treeOpen: open,
      treeFocus: treeKey.object(connId, obj.schema, obj.name),
      sidebarPanel: "connections",
      catalogSearch: { ...s.catalogSearch, query: "" },
    }));
  },
  openCatalogSearch: (scope) =>
    set((s) => ({
      sidebarPanel: "connections",
      catalogSearch: {
        query: s.catalogSearch.query,
        scope: scope === undefined ? s.catalogSearch.scope : scope,
        focusSeq: s.catalogSearch.focusSeq + 1,
      },
    })),
  setCatalogSearch: (patch) => set((s) => ({ catalogSearch: { ...s.catalogSearch, ...patch } })),

  refreshConnections: async () => {
    const connections = await api.listConnections();
    set({ connections });
  },
  openConnectionDialog: (profile, folderId) => set({ connectionDialog: { open: true, profile, folderId: folderId ?? null } }),
  closeConnectionDialog: () => set({ connectionDialog: { open: false } }),

  loadSchemas: async (connId, force = false) => {
    const cached = get().schemas[connId];
    if (cached && !force) return cached;
    const schemas = await api.listSchemas(connId);
    set((s) => ({ schemas: { ...s.schemas, [connId]: schemas } }));
    get()
      .refreshConnections()
      .catch(() => {});
    return schemas;
  },
  loadObjects: async (connId, schema, force = false) => {
    const key = `${connId}|${schema}`;
    const cached = get().objects[key];
    if (cached && !force) return cached;
    const objs = await api.listObjects(connId, schema);
    set((s) => ({ objects: { ...s.objects, [key]: objs } }));
    return objs;
  },
  loadColumns: async (connId, schema, name) => {
    const key = `${connId}|${schema}|${name}`;
    const cached = get().columns[key];
    if (cached) return cached;
    const d = await api.describeObject(connId, schema, name);
    set((s) => ({ columns: { ...s.columns, [key]: d.columns } }));
    return d.columns;
  },
  deleteConnection: (connId) =>
    new Promise<boolean>((resolve) => {
      const conn = get().connections.find((c) => c.id === connId);
      if (!conn) return resolve(false);
      const tabs = get().tabs.filter((t) => t.connection_id === connId).length;
      get().askConfirm({
        title: `Delete connection "${conn.name}"?`,
        reasons: [
          "Removes the connection, its saved password/SSH/OAuth credentials and its AI knowledge (indexed schema and notes).",
          "Saved queries, history and notebooks are kept, without a connection.",
          ...(tabs ? [`${tabs} open tab${tabs === 1 ? "" : "s"} will have no connection.`] : []),
          ...(conn.env === "prod" ? ["This is a production connection."] : []),
        ],
        confirmLabel: "Delete",
        onCancel: () => resolve(false),
        onConfirm: async () => {
          try {
            await api.deleteConnection(connId);
          } catch (e) {
            // "partial": deleted, but a secret could not be removed.
            const err = toError(e);
            get().toast(err.message, err.kind === "partial" ? "info" : "error");
            if (err.kind !== "partial") return resolve(false);
          }
          const drop = <T,>(m: Record<string, T>) => Object.fromEntries(Object.entries(m).filter(([k]) => k !== connId && !k.startsWith(connId + "|")));
          if (get().schemaFilter[connId]) get().setSchemaFilter(connId, null);
          set((s) => ({
            schemas: drop(s.schemas),
            objects: drop(s.objects),
            columns: drop(s.columns),
            tabs: s.tabs.map((t) => (t.connection_id === connId ? { ...t, connection_id: null } : t)),
            catalogSearch: s.catalogSearch.scope === connId ? { ...s.catalogSearch, scope: null } : s.catalogSearch,
          }));
          await get().refreshConnections();
          get().toast(`Deleted "${conn.name}"`, "success");
          resolve(true);
        },
      });
    }),
  disconnect: async (connId) => {
    await api.disconnect(connId);
    set((s) => {
      const drop = <T,>(m: Record<string, T>) =>
        Object.fromEntries(Object.entries(m).filter(([k]) => !k.startsWith(connId)));
      return { schemas: drop(s.schemas), objects: drop(s.objects), columns: drop(s.columns) };
    });
    await get().refreshConnections();
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
    };
    const tabs = [...s.tabs, tab];
    set({ tabs, activeTabId: tab.id });
    persistTabs(tabs);
    if (isTauri()) api.setSetting("active_tab", tab.id).catch(() => {});
    return tab.id;
  },
  closeTab: (id) => {
    const s = get();
    const idx = s.tabs.findIndex((t) => t.id === id);
    if (idx === -1) return;
    if (isTauri() && !s.tabs[idx].notebook_id && !s.tabs[idx].output_ref) api.closeTab(id).catch(() => {});
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
    set({ tabs, activeTabId, runs });
    persistTabs(tabs);
  },
  setActiveTab: (id) => {
    set({ activeTabId: id });
    if (isTauri()) api.setSetting("active_tab", id).catch(() => {});
  },
  updateTab: (id, patch) => {
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

  runTab: async (tabId, mode, input) => {
    const s = get();
    const tab = s.tabs.find((t) => t.id === tabId);
    if (!tab || tab.notebook_id || tab.output_ref) return;
    if (!s.backendAvailable) {
      s.toast("Queries can only run inside the DataBrain desktop app", "error");
      return;
    }
    const conn = s.connections.find((c) => c.id === tab.connection_id);
    if (!conn) {
      s.toast("Choose a connection for this tab first", "error");
      return;
    }
    if (s.runs[tabId]?.running) return;

    let sql: string;
    let base = 0;
    if (input.selTo > input.selFrom) {
      sql = input.doc.slice(input.selFrom, input.selTo);
      base = input.selFrom;
    } else if (mode === "all") {
      sql = input.doc;
    } else {
      const span = await api.statementAtCursor(conn.config.kind, input.doc, input.cursor);
      if (!span) {
        s.toast("Nothing to run", "info");
        return;
      }
      sql = span.sql;
      base = span.start;
    }
    if (!sql.trim()) {
      s.toast("Nothing to run", "info");
      return;
    }

    await get().runSql(tabId, conn.id, sql, base);
  },

  runSql: async (runKey, connectionId, sql, base = 0, sessionKey, outputName) => {
    const conn = get().connections.find((c) => c.id === connectionId);
    const submit = async (confirmed: boolean): Promise<boolean> => {
      try {
        const resp = await api.runQuery({
          connection_id: connectionId,
          tab_id: runKey,
          sql,
          base_offset: base,
          row_limit: get().rowLimit > 0 ? get().rowLimit : null,
          confirmed,
          session_key: sessionKey ?? null,
          output_name: outputName ?? null,
        });
        if (resp.status === "needs_confirmation") {
          return await new Promise<boolean>((resolve) =>
            get().askConfirm({
              title: "Confirm execution",
              reasons: resp.reasons,
              confirmLabel: "Run anyway",
              onConfirm: () => void submit(true).then(resolve),
              onCancel: () => resolve(false),
            }),
          );
        }
        set((st) => ({
          runs: {
            ...st.runs,
            [runKey]: {
              jobId: resp.job_id,
              running: true,
              startedAt: Date.now(),
              statements: resp.statements.map((p) => ({ plan: p, status: "pending", notices: [] })),
              activeIndex: null,
              connectionId,
            },
          },
        }));
        const early = earlyEvents.get(resp.job_id);
        if (early) {
          earlyEvents.delete(resp.job_id);
          for (const ev of early.events) get().handleJobEvent(ev);
        }
        return true;
      } catch (e) {
        const err = toError(e);
        if (err.kind === "reauth_required" && conn) {
          // Interactive sign-in expired: sign in, then retry once.
          if (await get().signInConnection(conn.id)) return submit(confirmed);
          return false;
        }
        get().toast(err.message, "error");
        return false;
      }
    };
    return submit(false);
  },

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
  newNotebook: async (connectionId) => {
    const s = get();
    const conn =
      connectionId !== undefined ? connectionId : (s.tabs.find((t) => t.id === s.activeTabId)?.connection_id ?? s.connections[0]?.id ?? null);
    let n = 1;
    while (s.notebooks.some((x) => x.name === `Notebook ${n}`)) n++;
    try {
      const nb = await api.saveNotebook({
        id: "",
        name: `Notebook ${n}`,
        connection_id: conn,
        folder_id: null,
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
  signInConnection: async (connectionId) => {
    const conn = get().connections.find((c) => c.id === connectionId);
    set({ signIn: { connectionId, label: `Sign in to ${conn?.name ?? "connection"}`, event: null } });
    try {
      const st = await api.signIn(connectionId);
      set({ signIn: null });
      get().toast(`Signed in${st.identity ? ` as ${st.identity}` : ""}`, "success");
      await get().refreshConnections();
      return true;
    } catch (e) {
      set({ signIn: null });
      const err = toError(e);
      if (err.kind !== "cancelled") get().toast(err.message, "error");
      return false;
    }
  },
  setSignIn: (signIn) => set({ signIn }),
  setSettingsOpen: (settingsOpen) => set({ settingsOpen }),

  cancelTab: async (tabId) => {
    const run = get().runs[tabId];
    if (!run?.jobId || !run.running) return;
    const jobId = run.jobId;
    const found = await api.cancelQuery(jobId).catch(() => false);
    if (found) return;
    // The backend no longer knows this job (it already ended, and its final
    // event was lost): mark the run as finished so the UI is never stuck.
    set((s) => {
      const r = s.runs[tabId];
      if (!r || r.jobId !== jobId || !r.running) return {};
      return {
        runs: {
          ...s.runs,
          [tabId]: {
            ...r,
            running: false,
            finishedStatus: "cancelled",
            durationMs: Date.now() - r.startedAt,
            statements: r.statements.map((x) =>
              x.status === "pending" || x.status === "running" ? { ...x, status: "cancelled" } : x,
            ),
          },
        },
      };
    });
  },

  setActiveStatement: (tabId, index) =>
    set((s) => {
      const run = s.runs[tabId];
      if (!run) return {};
      return { runs: { ...s.runs, [tabId]: { ...run, activeIndex: index } } };
    }),

  handleJobEvent: (e) => {
    if (e.type === "outputs_changed") {
      clearTimeout(outputsTimer);
      outputsTimer = setTimeout(() => void get().refreshOutputs(), 120);
      return;
    }
    const cur = get().runs[e.tab_id];
    if (!cur || cur.jobId !== e.job_id) {
      // Either stale (older job) or early (response not received yet).
      bufferEarly(e);
      return;
    }
    set((s) => {
      const run = s.runs[e.tab_id];
      if (!run || run.jobId !== e.job_id) return {};
      const statements = run.statements.slice();
      const upd = (i: number, patch: Partial<StatementRun>) => {
        if (statements[i]) statements[i] = { ...statements[i], ...patch };
      };
      let next: TabRun = { ...run, statements };
      switch (e.type) {
        case "statement_started": {
          upd(e.index, { status: "running", startedAt: Date.now() });
          // Still running after the slow-query threshold: look for why now,
          // on another session, instead of waiting for it to end.
          const ms = get().slowQuerySeconds * 1000;
          if (ms > 0) {
            const { tab_id, job_id, index } = e;
            setTimeout(() => {
              const r = get().runs[tab_id];
              if (r?.jobId === job_id && r.statements[index]?.status === "running") void get().computeTips(tab_id, job_id);
            }, ms + 50);
          }
          break;
        }
        case "progress":
          upd(e.index, { progressRows: e.rows });
          break;
        case "statement_finished":
          upd(e.index, {
            status: "done",
            result: e.result ?? undefined,
            output: e.output ?? undefined,
            rowsAffected: e.rows_affected,
            durationMs: e.duration_ms,
            notices: e.notices,
          });
          if (e.result) next.activeIndex = e.index;
          break;
        case "statement_failed": {
          const cancelled = e.error.kind === "cancelled";
          if (e.error.code === "oracle_client_missing") setTimeout(() => set({ oracleClientPrompt: true }), 0);
          upd(e.index, {
            status: cancelled ? "cancelled" : "error",
            error: e.error,
            durationMs: e.duration_ms,
          });
          const st = statements[e.index];
          if (!cancelled && st) next.errorRange = errorRange(st.plan, e.error.position);
          if (next.activeIndex === null || !cancelled) next.activeIndex = e.index;
          break;
        }
        case "job_finished":
          next = {
            ...next,
            running: false,
            finishedStatus: e.status,
            durationMs: e.duration_ms,
            statements: statements.map((x) =>
              x.status === "pending" || x.status === "running"
                ? { ...x, status: e.status === "cancelled" ? "cancelled" : x.status }
                : x,
            ),
          };
          if (next.activeIndex === null && statements.length > 0) {
            next.activeIndex = statements.length - 1;
          }
          if (e.status !== "cancelled") setTimeout(() => void get().computeTips(e.tab_id, e.job_id), 0);
          return {
            runs: { ...s.runs, [e.tab_id]: next },
            historyVersion: s.historyVersion + 1,
          };
      }
      return { runs: { ...s.runs, [e.tab_id]: next } };
    });
  },

  computeTips: async (runKey, jobId) => {
    const run = get().runs[runKey];
    if (!run || run.jobId !== jobId) return;
    const connId = run.connectionId;
    if (!connId) return;
    const conn = get().connections.find((c) => c.id === connId);
    const slow = slowStatements(run.statements, get().slowQuerySeconds * 1000, Date.now());
    if (!slow.length) return; // fast queries get no tips
    try {
      const tips = await computeRunTips(connId, conn?.config.kind, slow);
      set((s) => {
        const cur = s.runs[runKey];
        if (!cur || cur.jobId !== jobId) return {};
        return { runs: { ...s.runs, [runKey]: { ...cur, tips } } };
      });
    } catch {
      // Tips are best effort.
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
    const s = get();
    const tab = s.tabs.find((t) => t.id === tabId);
    if (!tab) return;
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
  setPaletteOpen: (paletteOpen) => set({ paletteOpen }),
  askConfirm: (confirm) => set({ confirm }),
}));

export const useActiveTab = () =>
  useStore((s) => s.tabs.find((t) => t.id === s.activeTabId) ?? null);
