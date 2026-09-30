// Typed wrappers around Tauri commands. Top-level argument names are
// camelCase (Tauri converts them to the Rust snake_case parameter names);
// nested struct fields keep their serde (snake_case) names.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
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
  KnowledgeEvent,
  KnowledgeView,
  KiroStatus,
  ModelInfo,
  Notebook,
  NotebookSummary,
  ProviderView,
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
    throw toError(e);
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
  ) => call<TestResult>("test_connection", { config, secret, connectionId, sshSecret }),
  signIn: (id: string) => call<AuthStatus>("sign_in", { id }),
  signOut: (id: string) => call<void>("sign_out", { id }),
  authStatus: (id: string) => call<AuthStatus>("auth_status", { id }),
  cancelSignIn: () => call<void>("cancel_sign_in"),
  connect: (id: string) => call<string>("connect", { id }),
  disconnect: (id: string) => call<void>("disconnect", { id }),

  listSchemas: (id: string) => call<SchemaInfo[]>("list_schemas", { id }),
  listObjects: (id: string, schema: string) => call<DbObject[]>("list_objects", { id, schema }),
  describeObject: (id: string, schema: string, name: string) =>
    call<ObjectDetail>("describe_object", { id, schema, name }),

  statementAtCursor: (kind: ConnectorKind, text: string, cursor: number) =>
    call<Span | null>("statement_at_cursor", { kind, text, cursor }),
  runQuery: (req: RunRequest) => call<RunResponse>("run_query", { req }),
  cancelQuery: (jobId: string) => call<boolean>("cancel_query", { jobId }),
  closeTab: (tabId: string) => call<void>("close_tab", { tabId }),

  resultInfo: (resultId: string) => call<ResultInfo>("result_info", { resultId }),
  fetchPage: (resultId: string, view: ViewSpec, offset: number, limit: number) =>
    call<Page>("fetch_page", { resultId, view, offset, limit }),
  findInResult: (resultId: string, view: ViewSpec, query: string, limit: number) =>
    call<FindResult>("find_in_result", { resultId, view, query, limit }),
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
  fileScanSql: (path: string) => call<string>("file_scan_sql", { path }),

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
  knIndex: (connectionId: string) => call<void>("kn_index", { connectionId }),
  knClear: (connectionId: string) => call<void>("kn_clear", { connectionId }),
  knSaveNote: (note: KnNote) => call<KnNote>("kn_save_note", { note }),
  knDeleteNote: (id: string) => call<void>("kn_delete_note", { id }),

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

export function onKnowledgeEvent(handler: (e: KnowledgeEvent) => void): Promise<UnlistenFn> {
  return listen<KnowledgeEvent>("knowledge-event", (e) => handler(e.payload));
}
