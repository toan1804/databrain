// Typed wrappers around Tauri commands. Top-level argument names are
// camelCase (Tauri converts them to the Rust snake_case parameter names);
// nested struct fields keep their serde (snake_case) names.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  ImportAction,
  NotesImportPreview,
  ExcelSheet,
  TableLayout,
  AgentEvent,
  AgentRequest,
  AiMessageRecord,
  AiProviderRecord,
  AiSessionRecord,
  AiUiRequest,
  AuditEntry,
  AuthEvent,
  AuthStatus,
  Folder,
  FolderKind,
  KnNote,
  IndexPlan,
  CredentialStoreKind,
  OracleClientStatus,
  OracleInstallProgress,
  CredentialStoreView,
  MigrationReport,
  KnowledgeEvent,
  KnObjectPage,
  ObjectKind,
  KnowledgeView,
  KiroStatus,
  ModelInfo,
  Notebook,
  NotebookSummary,
  ProviderView,
  ChartData,
  ChartSpec,
  OutputInfo,
  ColumnStats,
  ConnectionConfig,
  ConnectionProfile,
  ConnectionView,
  ConnectorInfo,
  ConnectorKind,
  DbObject,
  EngineError,
  ExportOptions,
  FindResult,
  HistoryEntry,
  HistoryQuery,
  JobEvent,
  ObjectDetail,
  Page,
  ResultInfo,
  RunRequest,
  RunResponse,
  SavedQuery,
  SchemaInfo,
  CatalogInfo,
  CachedExplorer,
  CachedListing,
  CachedColumns,
  SchemaRefresh,
  ExplorerRevalidation,
  Span,
  TabState,
  TestResult,
  ViewSpec,
} from "./types";

export const isTauri = (): boolean =>
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** Normalize any rejection into an EngineError. */
export function toError(e: unknown): EngineError {
  if (e && typeof e === "object" && "message" in e) {
    const o = e as Partial<EngineError>;
    return { kind: o.kind ?? "internal", message: String(o.message), code: o.code, position: o.position };
  }
  return { kind: "internal", message: String(e) };
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    const err = toError(e);
    // Any command can hit a missing Oracle client (connect, explorer, test).
    if (err.code === "oracle_client_missing" && typeof window !== "undefined") window.dispatchEvent(new CustomEvent("db:oracle-client-missing"));
    throw err;
  }
}

export const api = {
  listConnectors: () => call<ConnectorInfo[]>("list_connectors"),
  listConnections: () => call<ConnectionView[]>("list_connections"),
  /** `extraSecrets`: keychain slots (`ssh`, `client_secret`, `passphrase`); "" deletes. */
  saveConnection: (
    profile: ConnectionProfile,
    secret: string | null,
    clearSecret = false,
    extraSecrets: Record<string, string> = {},
  ) =>
    call<ConnectionProfile>("save_connection", {
      args: { profile, secret, clear_secret: clearSecret, extra_secrets: extraSecrets },
    }),
  deleteConnection: (id: string) => call<void>("delete_connection", { id }),
  testConnection: (
    config: ConnectionConfig,
    secret: string | null,
    connectionId: string | null,
    sshSecret: string | null = null,
    testId: string | null = null,
  ) => call<TestResult>("test_connection", { config, secret, connectionId, sshSecret, testId }),
  /** Stop a running test (the dialog's Cancel button). */
  cancelTestConnection: (testId: string) => call<boolean>("cancel_test_connection", { testId }),
  signIn: (id: string) => call<AuthStatus>("sign_in", { id }),
  signOut: (id: string) => call<void>("sign_out", { id }),
  authStatus: (id: string) => call<AuthStatus>("auth_status", { id }),
  cancelSignIn: () => call<void>("cancel_sign_in"),
  connect: (id: string) => call<string>("connect", { id }),
  disconnect: (id: string) => call<void>("disconnect", { id }),

  listSchemas: (id: string) => call<SchemaInfo[]>("list_schemas", { id }),
  listObjects: (id: string, schema: string) => call<DbObject[]>("list_objects", { id, schema }),
  /** Catalogs of three-level engines (null for two-level ones). */
  listCatalogs: (id: string) => call<CatalogInfo[] | null>("list_catalogs", { id }),
  listCatalogSchemas: (id: string, catalog: string) => call<SchemaInfo[]>("list_catalog_schemas", { id, catalog }),
  /** Explorer cache (local, no network). */
  cachedExplorer: (id: string) => call<CachedExplorer>("cached_explorer", { id }),
  cachedObjects: (id: string, schema: string) => call<CachedListing | null>("cached_objects", { id, schema }),
  cachedColumns: (id: string, schema: string, name: string) => call<CachedColumns | null>("cached_columns", { id, schema, name }),
  clearExplorerCache: (id: string) => call<void>("clear_explorer_cache", { id }),
  /** Bring one schema up to date: only changed objects are fetched when the engine has versions. */
  refreshSchema: (id: string, schema: string) => call<SchemaRefresh>("refresh_schema", { id, schema }),
  /** One fingerprint query; changed cached schemas are refreshed table by table. */
  revalidateExplorer: (id: string, open: string[], force: boolean) => call<ExplorerRevalidation>("revalidate_explorer", { id, open, force }),
  searchObjects: (id: string, query: string, limit?: number) =>
    call<DbObject[]>("search_objects", { id, query, limit: limit ?? null }),
  /** Completion: tables from the local knowledge index (no network). */
  completeTablesLocal: (id: string, schema: string | null, query: string, limit: number) =>
    call<DbObject[]>("complete_tables_local", { id, schema, query, limit }),
  /** Completion: tables filtered on the server (one schema when given). */
  completeTables: (id: string, schema: string | null, query: string, limit: number) =>
    call<DbObject[]>("complete_tables", { id, schema, query, limit }),
  /** Completion: functions/procedures/packages seen before (no network). */
  completeRoutinesLocal: (id: string, schema: string | null, query: string, limit: number) =>
    call<DbObject[]>("complete_routines_local", { id, schema, query, limit }),
  /** Completion: functions/procedures/packages filtered on the server. */
  completeRoutines: (id: string, schema: string | null, query: string, limit: number) =>
    call<DbObject[]>("complete_routines", { id, schema, query, limit }),
  /** Full DDL of a table, view or routine (null when the engine can't produce it). */
  objectDdl: (id: string, schema: string, name: string, kind: ObjectKind) => call<string | null>("object_ddl", { id, schema, name, kind }),
  /** Functions/procedures of an Oracle package. */
  packageMembers: (id: string, schema: string, pkg: string) => call<DbObject[]>("package_members", { id, schema, package: pkg }),
  completeColumnsLocal: (id: string, schema: string, name: string) =>
    call<string[] | null>("complete_columns_local", { id, schema, name }),
  describeObject: (id: string, schema: string, name: string) =>
    call<ObjectDetail>("describe_object", { id, schema, name }),
  tableLayout: (id: string, schema: string, name: string) => call<TableLayout>("table_layout", { id, schema, name }),

  statementAtCursor: (kind: ConnectorKind, text: string, cursor: number) =>
    call<Span | null>("statement_at_cursor", { kind, text, cursor }),
  runQuery: (req: RunRequest) => call<RunResponse>("run_query", { req }),
  cancelQuery: (jobId: string) => call<boolean>("cancel_query", { jobId }),
  closeTab: (tabId: string) => call<void>("close_tab", { tabId }),

  resultInfo: (resultId: string) => call<ResultInfo>("result_info", { resultId }),
  fetchPage: (resultId: string, view: ViewSpec, offset: number, limit: number) =>
    call<Page>("fetch_page", { resultId, view, offset, limit }),
  findInResult: (resultId: string, view: ViewSpec, query: string, limit: number, columns?: number[] | null) =>
    call<FindResult>("find_in_result", { resultId, view, query, limit, columns: columns ?? null }),
  columnStats: (resultId: string, view: ViewSpec, column: number) =>
    call<ColumnStats>("column_stats", { resultId, view, column }),
  exportResult: (resultId: string, view: ViewSpec, options: ExportOptions, path: string) =>
    call<number>("export_result", { resultId, view, options, path }),
  copyRows: (
    resultId: string,
    view: ViewSpec,
    offset: number,
    limit: number,
    options: ExportOptions,
  ) => call<string>("copy_rows", { resultId, view, offset, limit, options }),
  releaseResult: (resultId: string) => call<boolean>("release_result", { resultId }),

  listSavedQueries: (search: string | null) =>
    call<SavedQuery[]>("list_saved_queries", { search }),
  saveQuery: (query: SavedQuery) => call<SavedQuery>("save_query", { query }),
  deleteSavedQuery: (id: string) => call<void>("delete_saved_query", { id }),

  listFolders: (kind: FolderKind) => call<Folder[]>("list_folders", { kind }),
  saveFolder: (folder: Folder) => call<Folder>("save_folder", { folder }),
  deleteFolder: (id: string) => call<void>("delete_folder", { id }),
  moveToFolder: (kind: FolderKind, itemId: string, folderId: string | null) =>
    call<void>("move_to_folder", { kind, itemId, folderId }),

  listNotebooks: () => call<NotebookSummary[]>("list_notebooks"),
  getNotebook: (id: string) => call<Notebook>("get_notebook", { id }),
  saveNotebook: (notebook: Notebook) => call<Notebook>("save_notebook", { notebook }),
  deleteNotebook: (id: string) => call<void>("delete_notebook", { id }),
  fileScanSql: (path: string, allSheets = false) => call<string>("file_scan_sql", { path, allSheets }),
  excelSheets: (path: string) => call<ExcelSheet[]>("excel_sheets", { path }),

  listOutputs: () => call<OutputInfo[]>("list_outputs"),
  getOutput: (reference: string) => call<OutputInfo>("get_output", { reference }),
  dropOutput: (reference: string) => call<OutputInfo>("drop_output", { reference }),
  dropUnpinnedOutputs: () => call<number>("drop_unpinned_outputs"),
  loadOutput: (reference: string) => call<OutputInfo>("load_output", { reference }),
  renameOutput: (handle: string, name: string | null) => call<OutputInfo>("rename_output", { handle, name }),
  pinOutput: (handle: string, pinned: boolean) => call<OutputInfo>("pin_output", { handle, pinned }),
  outputDiffSql: (before: string, after: string, keys: string[], mapping: [string, string][] = []) =>
    call<string>("output_diff_sql", { before, after, keys, mapping }),
  resultsConnection: () => call<string>("results_connection"),
  chartData: (resultId: string, view: ViewSpec, spec: ChartSpec) => call<ChartData>("chart_data", { resultId, view, spec }),

  aiListProviders: () => call<ProviderView[]>("ai_list_providers"),
  aiSaveProvider: (record: AiProviderRecord, apiKey?: string | null) =>
    call<AiProviderRecord>("ai_save_provider", { args: { record, api_key: apiKey ?? null } }),
  aiDeleteProvider: (id: string) => call<void>("ai_delete_provider", { id }),
  aiListModels: (id: string) => call<ModelInfo[]>("ai_list_models", { id }),
  aiProviderSignIn: (id: string) => call<void>("ai_provider_sign_in", { id }),
  aiProviderSignOut: (id: string) => call<void>("ai_provider_sign_out", { id }),
  aiProviderStatus: (id: string) => call<KiroStatus>("ai_provider_status", { id }),
  aiSend: (args: AgentRequest) => call<string>("ai_send", { args }),
  aiCancel: (runId: string) => call<boolean>("ai_cancel", { runId }),
  aiRespond: (requestId: string, response: unknown) => call<boolean>("ai_respond", { requestId, response }),
  aiSessions: (connectionId: string | null) => call<AiSessionRecord[]>("ai_sessions", { connectionId }),
  aiSessionMessages: (sessionId: string) => call<AiMessageRecord[]>("ai_session_messages", { sessionId }),
  aiDeleteSession: (id: string) => call<void>("ai_delete_session", { id }),
  aiAudit: () => call<AuditEntry[]>("ai_audit"),
  mcpConfig: () => call<unknown>("mcp_config"),

  knGet: (connectionId: string) => call<KnowledgeView>("kn_get", { connectionId }),
  /** Indexed objects matching `filter` (schema.name or comment), one page at a time. */
  knObjects: (connectionId: string, filter: string, offset: number, limit: number) =>
    call<KnObjectPage>("kn_objects", { connectionId, filter, offset, limit }),
  oracleClientStatus: (libDir?: string | null) => call<OracleClientStatus>("oracle_client_status", { libDir: libDir || null }),
  oracleInstallClient: () => call<OracleClientStatus>("oracle_install_client"),
  oracleOpenDownload: () => call<void>("oracle_open_download"),
  oracleCancelInstall: () => call<void>("oracle_cancel_install"),
  credentialStore: () => call<CredentialStoreView>("credential_store"),
  setCredentialStore: (kind: CredentialStoreKind) => call<MigrationReport>("set_credential_store", { kind }),
  knPlan: (connectionId: string) => call<IndexPlan>("kn_plan", { connectionId }),
  knCancel: (connectionId: string) => call<boolean>("kn_cancel", { connectionId }),
  /** Index in the background; unchanged schemas are skipped unless `full`. */
  knIndex: (connectionId: string, scope?: string[] | null, batch?: number | null, full = false) =>
    call<void>("kn_index", { connectionId, scope: scope ?? null, batch: batch ?? null, full }),
  knClear: (connectionId: string) => call<void>("kn_clear", { connectionId }),
  knSaveNote: (note: KnNote) => call<KnNote>("kn_save_note", { note }),
  knDeleteNote: (id: string) => call<void>("kn_delete_note", { id }),
  hintLayouts: (connectionId: string, tables: string[]) =>
    call<{ written: string; schema?: string | null; name?: string | null; layout?: TableLayout | null; error?: string | null }[]>("hint_layouts", { connectionId, tables }),
  knCheckTarget: (connectionId: string, target: string) =>
    call<{ ok: boolean; target?: string | null; error?: string | null }>("kn_check_target", { connectionId, target }),
  knExportNotes: (connectionId: string, path: string) => call<number>("kn_export_notes", { connectionId, path }),
  knReadNotesFile: (connectionId: string, path: string) => call<NotesImportPreview>("kn_read_notes_file", { connectionId, path }),
  knImportNotes: (connectionId: string, actions: ImportAction[]) => call<number>("kn_import_notes", { connectionId, actions }),

  listHistory: (query: HistoryQuery) => call<HistoryEntry[]>("list_history", { query }),
  clearHistory: () => call<void>("clear_history"),

  loadTabs: () => call<TabState[]>("load_tabs"),
  saveTabs: (tabs: TabState[]) => call<void>("save_tabs", { tabs }),

  getSettings: () => call<Record<string, unknown>>("get_settings"),
  setSetting: (key: string, value: unknown) => call<void>("set_setting", { key, value }),
};

export function onJobEvent(handler: (e: JobEvent) => void): Promise<UnlistenFn> {
  return listen<JobEvent>("job-event", (e) => handler(e.payload));
}

type AiChannelPayload = AgentEvent | AiUiRequest;

export function onAiEvent(handler: (e: AiChannelPayload) => void): Promise<UnlistenFn> {
  return listen<AiChannelPayload>("ai-event", (e) => handler(e.payload));
}

export function onAuthEvent(handler: (e: AuthEvent) => void): Promise<UnlistenFn> {
  return listen<AuthEvent>("auth-event", (e) => handler(e.payload));
}

/** First-use download of the thin Oracle driver. */
export type OracleAgentEvent = { type: "progress"; done: number; total: number | null } | { type: "finished"; ok: boolean };
export function onOracleAgent(handler: (e: OracleAgentEvent) => void): Promise<UnlistenFn> {
  return listen<OracleAgentEvent>("oracle-agent", (e) => handler(e.payload));
}

export function onOracleInstall(handler: (e: OracleInstallProgress) => void): Promise<UnlistenFn> {
  return listen<OracleInstallProgress>("oracle-install", (e) => handler(e.payload));
}

export function onKnowledgeEvent(handler: (e: KnowledgeEvent) => void): Promise<UnlistenFn> {
  return listen<KnowledgeEvent>("knowledge-event", (e) => handler(e.payload));
}
