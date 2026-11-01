// Types of what the backend sends and receives (serde representations).
//
// Generated from the Rust types (ts-rs, `cargo test` writes ./bindings):
// change those, not these. The rest of this file is still written by hand.
import type { JobNodeKind } from "./bindings/JobNodeKind";
import type { FileFormat } from "./bindings/FileFormat";
import type { LoadMode } from "./bindings/LoadMode";
import type { NodeRunSummary } from "./bindings/NodeRunSummary";
import type { StepProgress } from "./bindings/StepProgress";
import type { RunLogEntry } from "./bindings/RunLogEntry";
import type { JobNode } from "./bindings/JobNode";
import type { JobEdge } from "./bindings/JobEdge";
import type { JobSchedule } from "./bindings/JobSchedule";
import type { Job } from "./bindings/Job";
import type { JobSummary } from "./bindings/JobSummary";
import type { NodeRunRecord } from "./bindings/NodeRunRecord";
import type { JobRun } from "./bindings/JobRun";
import type { ConnectorKind } from "./bindings/ConnectorKind";
import type { SslMode } from "./bindings/SslMode";
import type { EnvTag } from "./bindings/EnvTag";
import type { OAuthParams } from "./bindings/OAuthParams";
import type { AuthMethodKind } from "./bindings/AuthMethodKind";
import type { AuthMethod } from "./bindings/AuthMethod";
import type { SshAuth } from "./bindings/SshAuth";
import type { SshConfig } from "./bindings/SshConfig";
import type { RunQueryPolicy } from "./bindings/RunQueryPolicy";
import type { AiPolicy } from "./bindings/AiPolicy";
import type { ConnectionConfig } from "./bindings/ConnectionConfig";
import type { ConnectionProfile } from "./bindings/ConnectionProfile";
import type { CatalogInfo } from "./bindings/CatalogInfo";
import type { ColumnInfo } from "./bindings/ColumnInfo";
import type { ConnectorInfo } from "./bindings/ConnectorInfo";
import type { DbObject } from "./bindings/DbObject";
import type { FieldSpec } from "./bindings/FieldSpec";
import type { IndexInfo } from "./bindings/IndexInfo";
import type { ObjectDetail } from "./bindings/ObjectDetail";
import type { ObjectKind } from "./bindings/ObjectKind";
import type { SchemaInfo } from "./bindings/SchemaInfo";
import type { TableLayout } from "./bindings/TableLayout";
import type { ParamValue } from "./bindings/ParamValue";
import type { StatementKind } from "./bindings/StatementKind";
export type {
  JobNodeKind,
  FileFormat,
  LoadMode,
  NodeRunSummary,
  StepProgress,
  RunLogEntry,
  JobNode,
  JobEdge,
  JobSchedule,
  Job,
  JobSummary,
  NodeRunRecord,
  JobRun,
  ConnectorKind,
  SslMode,
  EnvTag,
  OAuthParams,
  AuthMethodKind,
  AuthMethod,
  SshAuth,
  SshConfig,
  RunQueryPolicy,
  AiPolicy,
  ConnectionConfig,
  ConnectionProfile,
  CatalogInfo,
  ColumnInfo,
  ConnectorInfo,
  DbObject,
  FieldSpec,
  IndexInfo,
  ObjectDetail,
  ObjectKind,
  SchemaInfo,
  TableLayout,
  ParamValue,
  StatementKind,
};

/** A `:name` parameter in the editor (UTF-16 offsets of `:name`). */
export interface ParamSpan {
  name: string;
  from: number;
  to: number;
  /** Oracle `&name` / `&&name`: replaced by the value as typed. */
  textual?: boolean;
}









export interface AuthStatus {
  signed_in: boolean;
  identity?: string | null;
  expires_at?: number | null;
}



export interface ConnectionView extends ConnectionProfile {
  connected: boolean;
}



export interface TestResult {
  server_version: string;
  latency_ms: number;
}



export interface ExplorerCacheState {
  checked_at: number | null;
  catalogs_at: number | null;
  schemas_at: number | null;
  listed_schemas: number;
}

/** What the explorer cache (local SQLite) holds for a connection. */
export interface CachedExplorer {
  catalogs: CatalogInfo[] | null;
  schemas: SchemaInfo[] | null;
  catalog_schemas: Record<string, SchemaInfo[]>;
  state: ExplorerCacheState;
}

export interface CachedListing {
  objects: DbObject[];
  listed_at: number;
}

export interface CachedColumns {
  columns: ColumnInfo[];
  /** The table changed (or may have) since: show, but describe again when online. */
  stale: boolean;
}

export interface SchemaRefresh {
  schema: string;
  mode: "unchanged" | "incremental" | "full";
  added: number;
  changed: number;
  removed: number;
  objects: DbObject[];
}

export interface ExplorerRevalidation {
  refreshed: SchemaRefresh[];
  unchanged: number;
  deferred: string[];
  fingerprints: boolean;
  skipped: boolean;
}




export interface ExcelSheet {
  name: string;
  /** Used range, e.g. `A1:G120`; absent for an empty sheet. */
  range?: string;
  hidden: boolean;
}





export interface PlannedStatement {
  index: number;
  sql: string;
  /** UTF-16 offsets into the editor document. */
  start: number;
  end: number;
  classification: { kind: StatementKind; missing_where: boolean; keyword: string };
}

// ---- lineage

export interface LineageRange {
  start: number;
  end: number;
}

export type LineageNodeKind = "table" | "cte" | "subquery" | "set_op" | "result";
export type LineageEdgeKind = "direct" | "transform" | "aggregate" | "filter" | "join";

export interface LineageColumn {
  name: string;
  span?: LineageRange;
  expr?: string;
}

export interface LineageNode {
  id: string;
  kind: LineageNodeKind;
  name: string;
  columns: LineageColumn[];
  columns_known: boolean;
  /** Created, inserted into, updated, merged into or deleted from by the script. */
  written: boolean;
  /** Created by the script as a temporary table. */
  temporary?: boolean;
  statement?: number;
  span?: LineageRange;
}

export interface LineageEdge {
  from: string;
  from_column?: string;
  to: string;
  to_column?: string;
  kind: LineageEdgeKind;
  span?: LineageRange;
  /** Filters/joins: the clause (`LEFT JOIN customers c ON c.id = o.customer_id`, `WHERE …`). */
  detail?: string;
  /** Statement (index) the edge comes from. */
  statement?: number;
}

/** Table-level chain: `from` (table or result) feeds, or decides the rows of, `to`. */
export interface LineageFlow {
  from: string;
  to: string;
  kind: LineageEdgeKind;
  /** Statement indexes on the way, in order. */
  statements: number[];
}

export interface LineageGraph {
  nodes: LineageNode[];
  edges: LineageEdge[];
  statements: { index: number; kind: string; range: LineageRange; query?: LineageRange; result?: string; error?: string }[];
  warnings: { message: string; span?: LineageRange }[];
  partial: boolean;
  unknown_tables: string[];
  /** The chain table to table, through CTEs/subqueries and across statements. */
  flows?: LineageFlow[];
}

export interface LineageCheck {
  index: number;
  status: "ok" | "error" | "unsupported" | "skipped" | "offline" | "timeout";
  message?: string;
  position?: number;
}

export interface LineageView {
  graph: LineageGraph;
  checks: LineageCheck[];
  described: string[];
}



export interface RunRequest {
  connection_id: string;
  tab_id: string;
  sql: string;
  base_offset: number;
  row_limit: number | null;
  confirmed: boolean;
  origin?: "user" | "ai" | "mcp" | "job";
  session_key?: string | null;
  /** Name the job's last result (`results.<name>` in the Results connection). */
  output_name?: string | null;
  /** Values of the statement's `:name` parameters. */
  params?: Record<string, ParamValue>;
}

export type RunResponse =
  | { status: "started"; job_id: string; statements: PlannedStatement[] }
  | { status: "needs_confirmation"; reasons: string[]; statements: PlannedStatement[] };

export interface EngineError {
  kind: string;
  message: string;
  code?: string;
  position?: number;
}

export type TypeFamily = "number" | "text" | "bool" | "date" | "time" | "binary" | "other";

export interface ColumnMeta {
  name: string;
  data_type: string;
  db_type: string | null;
  family: TypeFamily;
}

export interface ResultInfo {
  id: string;
  columns: ColumnMeta[];
  total_rows: number;
  complete: boolean;
  truncated: boolean;
  bytes: number;
}

export type RunStatus = "success" | "error" | "cancelled";

export type JobEvent =
  | { type: "statement_started"; job_id: string; tab_id: string; index: number; sql: string }
  | {
      type: "progress";
      job_id: string;
      tab_id: string;
      index: number;
      rows: number;
      elapsed_ms: number;
    }
  | {
      type: "statement_finished";
      job_id: string;
      tab_id: string;
      index: number;
      result: ResultInfo | null;
      output?: OutputInfo | null;
      rows_affected: number | null;
      duration_ms: number;
      notices: string[];
      /** Time on the server, when the download was reported separately. */
      server_ms?: number | null;
    }
  | {
      /** The server finished; the result is being downloaded (`elapsed_ms` = time on the server). */
      type: "downloading";
      job_id: string;
      tab_id: string;
      index: number;
      notice?: string | null;
      elapsed_ms: number;
    }
  | {
      type: "statement_failed";
      job_id: string;
      tab_id: string;
      index: number;
      error: EngineError;
      duration_ms: number;
      statement_start: number;
    }
  | { type: "job_finished"; job_id: string; tab_id: string; status: RunStatus; duration_ms: number; in_transaction?: boolean }
  | { type: "outputs_changed"; job_id: ""; tab_id: "" };

export type FilterOp =
  | "contains"
  | "not_contains"
  | "equals"
  | "not_equals"
  | "starts_with"
  | "ends_with"
  | "gt"
  | "gte"
  | "lt"
  | "lte"
  | "is_null"
  | "is_not_null";

export interface ColumnFilter {
  column: number;
  op: FilterOp;
  value: string;
}

export interface SortKey {
  column: number;
  descending: boolean;
}

export interface ViewSpec {
  filters: ColumnFilter[];
  quick_filter: string | null;
  sort: SortKey[];
}

export interface Page {
  view_rows: number;
  total_rows: number;
  offset: number;
  row_ids: number[];
  rows: (string | null)[][];
}

export interface FindResult {
  matches: { row: number; col: number }[];
  truncated: boolean;
}

export interface ColumnStats {
  column: number;
  count: number;
  nulls: number;
  distinct: number;
  min: string | null;
  max: string | null;
  top: { value: string | null; count: number }[];
}

export type OutputState = "live" | "on_disk" | "evicted";

export interface OutputInfo {
  handle: string;
  name: string | null;
  /** Older version of a named output: [name, versions back]. */
  version_of: [string, number] | null;
  result_id: string;
  connection_id: string;
  connection_name: string;
  kind: ConnectorKind;
  sql: string;
  tab_id: string;
  statement_index: number;
  created_at: number;
  rows: number;
  columns: ColumnMeta[];
  truncated: boolean;
  row_limit: number | null;
  bytes: number;
  pinned: boolean;
  state: OutputState;
  origin: "user" | "ai" | "mcp" | "job";
  last_used: number;
  active: boolean;
}

export type ChartAgg = "sum" | "avg" | "count" | "min" | "max" | "none";

export interface ChartSpec {
  x: number;
  y: number[];
  agg: ChartAgg;
  series?: number | null;
  limit?: number;
}

export interface ChartData {
  x: string[];
  x_family: "number" | "date" | "time" | "text";
  series: { name: string; values: (number | null)[] }[];
  truncated: boolean;
  rows: number;
}

export type ExportFormat = "csv" | "tsv" | "json" | "ndjson" | "markdown" | "sql_insert" | "parquet" | "xlsx";

/** Count / distinct / sum… of the selected cells in a result grid. */
export interface SelectionSummary {
  cells: number;
  non_null: number;
  distinct: number;
  distinct_capped: boolean;
  numbers: number;
  sum: number | null;
  min: number | null;
  max: number | null;
  avg: number | null;
}

export interface ExportOptions {
  format: ExportFormat;
  header: boolean;
  table_name?: string | null;
  dialect?: ConnectorKind | null;
  columns?: number[];
  /** CSV/TSV: prefix text that Excel would run as a formula with '. */
  excel_safe?: boolean;
}

export interface SavedQuery {
  id: string;
  name: string;
  sql: string;
  connection_id?: string | null;
  folder_id?: string | null;
  description?: string | null;
  tags: string[];
  ai_example?: boolean;
  created_at: number;
  updated_at: number;
}

export interface HistoryEntry {
  id: number;
  connection_id: string | null;
  connection_name: string | null;
  sql: string;
  started_at: number;
  duration_ms: number;
  rows: number | null;
  status: RunStatus;
  error: string | null;
  /** Who ran it: you, the AI assistant, an MCP client or a job. */
  origin?: "user" | "ai" | "mcp" | "job";
  output_handle?: string | null;
  result_id?: string | null;
}

export interface HistoryQuery {
  search?: string | null;
  connection_id?: string | null;
  limit?: number | null;
  before?: number | null;
  /** Leave out statements run by the assistant (AI panel, MCP). */
  exclude_ai?: boolean;
}

export interface TabState {
  id: string;
  title: string;
  sql: string;
  connection_id?: string | null;
  saved_query_id?: string | null;
  notebook_id?: string | null;
  output_ref?: string | null;
  /** false: manual commit (writes wait for Commit/Rollback). */
  auto_commit?: boolean | null;
  /** The .sql file on disk the tab edits. */
  file_path?: string | null;
}

export interface SqlFile {
  path: string;
  text: string;
  mtime_ms: number;
}

export interface Span {
  start: number;
  end: number;
  sql: string;
}

export type FolderKind = "connections" | "queries" | "notebooks";

export interface Folder {
  id: string;
  parent_id?: string | null;
  name: string;
  kind: FolderKind;
}

// ---- notebooks

export type CellKind = "sql" | "markdown";

export interface CellRunSummary {
  finished_at: number;
  duration_ms: number;
  rows?: number | null;
  error?: string | null;
}

export interface NotebookCell {
  id: string;
  kind: CellKind;
  source: string;
  /** Output name: later cells query it as results.<name>. */
  output_name?: string | null;
  connection_id?: string | null;
  collapsed?: boolean;
  /** Editor height in px (unset = grows with the SQL). */
  editor_height?: number | null;
  /** Output height in px (unset = 300). */
  output_height?: number | null;
  last_run?: CellRunSummary | null;
}

export interface Notebook {
  id: string;
  name: string;
  connection_id?: string | null;
  folder_id?: string | null;
  cells: NotebookCell[];
  created_at: number;
  updated_at: number;
}

export interface NotebookSummary {
  id: string;
  name: string;
  connection_id?: string | null;
  folder_id?: string | null;
  cell_count: number;
  updated_at: number;
}

// ---- AI

export type ProviderKind =
  | "openai"
  | "anthropic"
  | "gemini"
  | "azure_openai"
  | "openrouter"
  | "ollama"
  | "lm_studio"
  | "openai_compatible"
  | "kiro";

export type ProviderAuth = "api_key" | "browser_openrouter" | "azure_cli" | "google_adc" | "kiro_browser" | "none";

export interface KiroStatus {
  installed: boolean;
  cli_path?: string | null;
  signed_in: boolean;
  account_type?: string | null;
  identity?: string | null;
  message?: string | null;
}

export interface ProviderConfig {
  base_url?: string | null;
  auth: ProviderAuth;
  default_model?: string | null;
  fast_model?: string | null;
  api_version?: string | null;
  extra_headers?: Record<string, string>;
  max_output_tokens?: number | null;
  cli_env?: Record<string, string>;
  agents_dir?: string | null;
}

export interface AiProviderRecord {
  id: string;
  kind: ProviderKind;
  name: string;
  config: ProviderConfig;
  created_at: number;
  updated_at: number;
}

export interface ProviderView extends AiProviderRecord {
  has_key: boolean;
}

export interface ModelInfo {
  id: string;
  name?: string | null;
}

export type AiMode = "chat" | "generate" | "edit" | "fix_error" | "explain" | "analyze_result" | "optimize";

export interface UiContext {
  editor_sql?: string | null;
  selection?: string | null;
  last_error?: string | null;
  /** The statement that ran and failed (Fix error). */
  failed_sql?: string | null;
  /** Its 1-based line in the editor, when found there. */
  failed_line?: number | null;
  /** Other connections the assistant may use for this question (ids). */
  connections?: string[];
  /** Job open in the active tab (the user is asking about it). */
  open_job_id?: string | null;
  open_job_name?: string | null;
  result_id?: string | null;
  /** Not about this connection's data (a connection test error): the assistant doesn't connect, read the schema or use tools. */
  no_database?: boolean;
  mentions?: string[];
}

export interface AgentRequest {
  session_id?: string | null;
  connection_id: string;
  provider_id?: string | null;
  model?: string | null;
  message: string;
  mode: AiMode;
  context: UiContext;
  tab_id?: string | null;
}

export interface AiError {
  kind: string;
  message: string;
}

interface Ev {
  session_id: string;
  run_id: string;
}

export type AgentEvent =
  | ({ type: "started" } & Ev)
  | ({ type: "text_delta"; text: string } & Ev)
  | ({ type: "tool_started"; call_id: string; tool: string; args: Record<string, unknown> } & Ev)
  | ({ type: "tool_finished"; call_id: string; tool: string; content: string; display: unknown } & Ev)
  | ({ type: "usage"; input: number; output: number } & Ev)
  /** Kiro: credits a turn spent and how full its context is (%). */
  | ({ type: "metering"; credits: number; context_percent: number | null } & Ev)
  | ({ type: "finished"; text: string } & Ev)
  | ({ type: "failed"; error: AiError } & Ev);

export interface EditProposal {
  sql: string;
  mode: "replace_selection" | "insert_at_cursor" | "replace_all" | "new_tab";
  title?: string | null;
}

/** UI requests from the agent (answered with `ai_respond`). */
export type AiUiRequest =
  | { type: "approval_request"; request_id: string; run_id: string; tool: string; summary: string; detail: Record<string, unknown> }
  | { type: "edit_proposal"; request_id: string; run_id: string; tab_id?: string | null; proposal: EditProposal }
  | { type: "editor_request"; request_id: string; run_id: string; tab_id?: string | null };

export interface AiSessionRecord {
  id: string;
  title: string;
  connection_id?: string | null;
  provider_id?: string | null;
  model?: string | null;
  created_at: number;
  updated_at: number;
}

export interface AiMessageRecord {
  id: number;
  session_id: string;
  role: "user" | "assistant" | "tool";
  content: { text?: string; tool_calls?: { id: string; name: string; arguments: unknown }[]; tool_name?: string } | string;
  tokens_in?: number | null;
  tokens_out?: number | null;
  created_at: number;
}

export interface AuditEntry {
  id: number;
  session_id?: string | null;
  connection_id?: string | null;
  tool: string;
  args: unknown;
  decision: string;
  summary?: string | null;
  created_at: number;
}

export interface DeviceCodePrompt {
  provider: string;
  user_code: string;
  verification_uri: string;
  verification_uri_complete?: string | null;
  message?: string | null;
}

export type AuthEvent =
  | { type: "browser_opened"; url: string }
  | { type: "device_code"; prompt: DeviceCodePrompt }
  | { type: "terminal_opened"; command: string; terminal?: string }
  /** No terminal could be opened (or the user runs it): show the command. */
  | { type: "run_command"; command: string; reason?: string | null }
  | { type: "finished" };

/** Settings → Terminal: "auto", "manual", "custom" or a preset id. */
export interface TerminalSetting {
  kind: string;
  custom: string;
}

export interface TerminalOption {
  id: string;
  label: string;
  /** Where it was found on this computer (found terminals only). */
  detail?: string | null;
}

export type TerminalLaunch = { type: "opened"; terminal: string } | { type: "manual"; command: string; reason?: string | null };

export interface KnObject {
  schema: string;
  name: string;
  kind: string;
  comment?: string | null;
  row_estimate?: number | null;
  columns: ColumnInfo[];
}

export interface KnNote {
  id: string;
  connection_id: string;
  target?: string | null;
  body: string;
  author: "user" | "ai" | string;
  status: "approved" | "proposed";
  created_at: number;
  /** AI update proposal: approving it replaces this note id. */
  replaces?: string | null;
}

/** A note in a shared notes file (no local ids). */
export interface NoteEntry {
  target?: string | null;
  body: string;
  author?: string | null;
}

export interface ImportItem {
  incoming: NoteEntry;
  kind: "new" | "same" | "conflict";
  existing: KnNote[];
  /** Tables of the note that this connection does not have. */
  invalid_target?: string | null;
}

export interface ImportAction {
  incoming: NoteEntry;
  action: "add" | "skip" | "replace" | "merge";
  existing_ids: string[];
  body?: string;
  target?: string | null;
}

export interface NotesImportPreview {
  source?: { connection: string; kind: string } | null;
  items: ImportItem[];
}

export interface KnState {
  connection_id: string;
  indexed_at: number;
  objects: number;
  schemas: string[];
  error?: string | null;
}

export interface KnowledgeView {
  state: KnState | null;
  notes: KnNote[];
}

/** A page of indexed objects (`kn_objects`). */
export interface KnObjectPage {
  objects: KnObject[];
  /** Objects matching the filter, all pages. */
  total: number;
}

export type KnowledgeEvent =
  | { type: "progress"; connection_id: string; schema: string; done: number; total: number }
  | {
      type: "finished";
      connection_id: string;
      report: { schemas: number; objects: number; changed: number; removed: number; errors: string[]; cancelled: boolean; skipped?: number };
    }
  | { type: "cancelled"; connection_id: string }
  | { type: "failed"; connection_id: string; error: string };

export interface OracleClientStatus {
  installed: boolean;
  version: string | null;
  lib_dir: string | null;
  message: string | null;
  platform: { os: string; arch: string; download_page: string; download_url: string | null; auto_install: boolean; note: string | null };
}

export type OracleInstallProgress =
  | { phase: "downloading"; received: number; total: number | null }
  | { phase: "installing" }
  | { phase: "verifying" };

export type CredentialStoreKind = "keychain" | "vault";
export interface CredentialStoreView {
  kind: CredentialStoreKind;
  vault_dir: string | null;
  switchable: boolean;
}
export interface MigrationReport {
  moved: number;
  failed: string[];
}

export interface PlanSchema {
  name: string;
  catalog?: string | null;
  is_default: boolean;
  system: boolean;
  objects: number | null;
  selected: boolean;
}

export interface IndexPlan {
  schemas: PlanSchema[];
  /** Saved scope; empty = never chosen, ["*"] = all. */
  scope: string[];
  catalogs: number;
  total_objects: number | null;
  large: boolean;
  /** Saved schemas-per-request setting. */
  batch: number;
}

// ------------------------------------------------------------------ jobs

export type NodeRunStatus = Exclude<NodeRunSummary["status"], "">;



export interface LoadCheck {
  label: string;
  sql: string;
  status: "ok" | "error" | "warning" | "unchecked";
  message?: string | null;
}

/** The table behind an editable result (see the backend's edit module). */
export interface EditTarget {
  connection_id: string;
  schema: string;
  table: string;
  /** Result column indexes of the primary key. */
  key_columns: number[];
  /** Per result column: the table column (null: read-only expression). */
  columns: (string | null)[];
  /** transaction | block (Oracle PL/SQL) | statements (no transactions). */
  apply: "transaction" | "block" | "statements";
}

export interface EditStatement {
  sql: string;
  expect_rows: number | null;
}

export interface ApplyReport {
  statements: number;
  rows: number;
  in_open_transaction: boolean;
}

/** A foreign key from (parent) or to (children) a result's table. */
export interface ResultLink {
  direction: "parent" | "children";
  schema: string;
  table: string;
  /** Result column indexes holding the values to look up. */
  columns: number[];
  target_columns: string[];
}

/** "Load into…" a table from the result grid. */
export interface LoadResultArgs {
  target_connection_id: string;
  table: string;
  mode: LoadMode;
  key_columns: string[];
  create_table: boolean;
}

/** Outcome of a load. */
export interface LoadReport {
  rows: number;
  created_table: boolean;
  transaction: boolean;
  notices: string[];
}

/** Dry run of a load step: nothing is kept. */
/** Which parts of a load target path exist (names compared exactly). */
export interface LoadTarget {
  resolved: string;
  catalog?: string | null;
  schema?: string | null;
  table: string;
  catalog_exists?: boolean | null;
  schema_exists?: boolean | null;
  table_exists?: boolean | null;
  /** Fix these first: missing catalog/schema, a view, too many name parts. */
  problems: string[];
  create_sql?: string;
  /** The path as the database spells it, when what was typed differs only in case. */
  suggestion?: string;
  notes: string[];
  /** A part was typed in another case: `resolved` has the database's spelling, and the load uses it. */
  respelled?: boolean;
}

export interface LoadDryRun {
  ok: boolean;
  /** transaction: ran on sample rows and rolled back; explain: checked one by one; none: not planned. */
  method: "transaction" | "explain" | "none";
  connection: string;
  table: string;
  table_exists?: boolean | null;
  creates_table: boolean;
  sample_rows?: number | null;
  rows_error?: string | null;
  checks: LoadCheck[];
  notices: string[];
  /** INSERT size the run uses. */
  batch?: { rows: number; bytes: number } | null;
}

/** INSERT batch sizes a connection type allows. */
export interface BatchLimits {
  default_rows: number;
  max_rows: number;
  default_bytes: number;
  max_bytes: number;
  rows_note: string;
  bytes_note: string;
}







export interface JobRunEvent {
  job_id: string;
  run: JobRun;
}


export interface JobLogEvent {
  job_id: string;
  run_id: number;
  entries: RunLogEntry[];
}


/** Account budget a provider reports (credits, balance, key limit). */
export interface BudgetMeter {
  label: string;
  used: number | null;
  limit: number | null;
  remaining: number | null;
  unit: string;
}

export interface Budget {
  plan: string | null;
  meters: BudgetMeter[];
  resets_on: string | null;
  manage_url: string | null;
}

export interface ProviderUsage {
  budget: Budget | null;
  budget_error: string | null;
  budget_supported: boolean;
  /** [input, output] tokens of DataBrain's conversations. */
  tokens_today: [number, number];
  tokens_month: [number, number];
}

/** DuckDB engine of this build: compiled in, or the library on the computer. */
export interface DuckdbEngineStatus {
  bundled: boolean;
  available: boolean;
  path?: string | null;
  version?: string | null;
  error?: string | null;
  tried: string[];
  /** libduckdb.dylib / libduckdb.so / duckdb.dll */
  file_name: string;
}

/** What "Clear history" deletes. */
export type HistoryClear = { kind: "all" } | { kind: "ai" } | { kind: "older_than_days"; days: number };

/** Thin Oracle driver: installed or not, and what to download by hand. */
export interface OracleAgentInfo {
  installed: string | null;
  file_name: string;
  url: string;
  sha256: string | null;
  release: string;
}

/** One step of a query plan (any engine; see the backend's plan module). */
export interface PlanNode {
  op: string;
  object?: string;
  cost?: number;
  rows?: number;
  actual_rows?: number;
  /** Inclusive time per loop (ms). */
  time_ms?: number;
  loops?: number;
  extra: [string, string][];
  children: PlanNode[];
}

export interface QueryPlan {
  analyzed: boolean;
  roots: PlanNode[];
  raw: string;
  planning_ms?: number;
  execution_ms?: number;
}

/** Where connections are imported from. */
export type ImportSource = "file" | "dbeaver" | "datagrip" | "pgpass";

export interface ImportCandidate {
  index: number;
  name: string;
  folder: string[];
  kind: ConnectorKind;
  target: string;
  user?: string | null;
  env: EnvTag;
  /** Name of an existing connection to the same place. */
  existing?: string | null;
  has_password: boolean;
  note?: string | null;
}

export interface ImportPreview {
  path: string;
  candidates: ImportCandidate[];
}

export interface ImportReport {
  imported: number;
  errors: string[];
}

export type DiffStatus = "only_source" | "only_target" | "changed" | "same";

export interface ColumnChange {
  column?: string | null;
  /** added | removed | type | nullable | default | primary_key | foreign_key */
  what: string;
  source?: string | null;
  target?: string | null;
}

export interface ObjectDiff {
  name: string;
  kind: ObjectKind;
  status: DiffStatus;
  changes: ColumnChange[];
}

export interface SchemaDiff {
  source_kind: ConnectorKind;
  target_kind: ConnectorKind;
  objects: ObjectDiff[];
  /** Makes the target like the source (target dialect); never run by the app. */
  script: string;
}

export interface UpdateInfo {
  current: string;
  latest: string;
  newer: boolean;
  url: string;
  download?: string | null;
  download_name?: string | null;
  notes?: string | null;
  published_at?: string | null;
}

/** Scheduled jobs while the app is closed (Settings → Jobs). */
export interface BackgroundStatus {
  enabled: boolean;
  installed: boolean;
  mechanism: string;
  location: string;
  last_tick?: number | null;
  log_file: string;
}
