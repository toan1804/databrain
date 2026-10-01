// Types mirroring the Rust backend (serde representations).

export type ConnectorKind =
  | "sqlite"
  | "postgres"
  | "mysql"
  | "mssql"
  | "oracle"
  | "snowflake"
  | "databricks"
  | "bigquery"
  | "duckdb";
export type SslMode = "disable" | "prefer" | "require" | "verify_full";
export type EnvTag = "none" | "dev" | "staging" | "prod";

export interface OAuthParams {
  client_id?: string;
  scopes?: string;
  redirect_port?: number;
  tenant?: string;
  user?: string;
  authorize_url?: string;
  token_url?: string;
}

export type AuthMethodKind =
  | "none"
  | "password"
  | "api_token"
  | "key_pair"
  | "oauth_browser"
  | "device_code"
  | "client_credentials"
  | "external_browser"
  | "cloud_cli"
  | "service_account";

export type AuthMethod =
  | { method: "none" }
  | { method: "password"; user: string }
  | { method: "api_token"; user?: string | null }
  | { method: "key_pair"; user: string }
  | ({ method: "oauth_browser" } & OAuthParams)
  | ({ method: "device_code" } & OAuthParams)
  | ({ method: "client_credentials" } & OAuthParams)
  | { method: "external_browser"; user: string }
  | { method: "cloud_cli"; profile?: string | null }
  | { method: "service_account" };

export type SshAuth = { method: "password" } | { method: "key"; path: string } | { method: "agent" };

export interface SshConfig {
  host: string;
  port: number;
  user: string;
  auth: SshAuth;
  host_key_fingerprint?: string | null;
}

export type RunQueryPolicy = "never" | "ask" | "auto_read";

export interface AiPolicy {
  ai_enabled: boolean;
  allowed_providers: string[];
  share_metadata: boolean;
  share_sample_values: boolean;
  share_result_rows: boolean;
  run_query: RunQueryPolicy;
  allow_write: boolean;
  max_rows_to_model: number;
  pii_columns: string[];
  index_schemas: string[];
  /** Schemas per metadata request while indexing (default 25). */
  index_batch?: number;
  mcp_enabled: boolean;
}

export interface AuthStatus {
  signed_in: boolean;
  identity?: string | null;
  expires_at?: number | null;
}

export interface ConnectionConfig {
  kind: ConnectorKind;
  host?: string | null;
  port?: number | null;
  database?: string | null;
  file_path?: string | null;
  auth: AuthMethod;
  ssl_mode: SslMode;
  read_only: boolean;
  options?: Record<string, string>;
  ssh?: SshConfig | null;
}

export interface ConnectionProfile {
  id: string;
  name: string;
  config: ConnectionConfig;
  color?: string | null;
  env: EnvTag;
  folder_id?: string | null;
  has_secret: boolean;
  ai_policy?: AiPolicy;
  created_at: number;
  updated_at: number;
}

export interface ConnectionView extends ConnectionProfile {
  connected: boolean;
}

export interface FieldSpec {
  key: string;
  label: string;
  placeholder?: string;
  required: boolean;
  help?: string;
}

export interface ConnectorInfo {
  kind: ConnectorKind;
  display_name: string;
  default_port: number | null;
  uses_file: boolean;
  auth_methods: AuthMethodKind[];
  capabilities: {
    transactions: boolean;
    cancel: boolean;
    schemas: boolean;
    read_only_sessions: boolean;
    ssh: boolean;
  };
  fields: FieldSpec[];
  note?: string;
}

export interface TestResult {
  server_version: string;
  latency_ms: number;
}

export interface SchemaInfo {
  /** Qualified id (`catalog.schema` for three-level engines). */
  name: string;
  is_default: boolean;
  /** Databricks catalog, Snowflake database, BigQuery project, DuckDB database. */
  catalog?: string | null;
}

export type ObjectKind =
  | "table"
  | "view"
  | "materialized_view"
  | "foreign_table"
  | "function"
  | "procedure"
  | "sequence"
  | "other";

export interface DbObject {
  schema: string;
  name: string;
  kind: ObjectKind;
  comment?: string;
  row_estimate?: number;
}

export interface ColumnInfo {
  name: string;
  data_type: string;
  nullable: boolean;
  is_primary_key: boolean;
  default?: string;
  comment?: string;
}

export interface ObjectDetail {
  object: DbObject;
  columns: ColumnInfo[];
  ddl?: string;
}

export type StatementKind =
  | "read"
  | "session"
  | "transaction"
  | "dml"
  | "ddl"
  | "other"
  | "unknown";

export interface PlannedStatement {
  index: number;
  sql: string;
  /** UTF-16 offsets into the editor document. */
  start: number;
  end: number;
  classification: { kind: StatementKind; missing_where: boolean; keyword: string };
}

export interface RunRequest {
  connection_id: string;
  tab_id: string;
  sql: string;
  base_offset: number;
  row_limit: number | null;
  confirmed: boolean;
  origin?: "user" | "ai" | "mcp";
  session_key?: string | null;
  /** Name the job's last result (`results.<name>` in the Results connection). */
  output_name?: string | null;
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
  | { type: "job_finished"; job_id: string; tab_id: string; status: RunStatus; duration_ms: number }
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
  origin: "user" | "ai" | "mcp";
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

export interface ExportOptions {
  format: ExportFormat;
  header: boolean;
  table_name?: string | null;
  dialect?: ConnectorKind | null;
  columns?: number[];
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
  output_handle?: string | null;
  result_id?: string | null;
}

export interface HistoryQuery {
  search?: string | null;
  connection_id?: string | null;
  limit?: number | null;
  before?: number | null;
}

export interface TabState {
  id: string;
  title: string;
  sql: string;
  connection_id?: string | null;
  saved_query_id?: string | null;
  notebook_id?: string | null;
  output_ref?: string | null;
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

export type AiMode = "chat" | "generate" | "edit" | "fix_error" | "explain" | "analyze_result";

export interface UiContext {
  editor_sql?: string | null;
  selection?: string | null;
  last_error?: string | null;
  result_id?: string | null;
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
  | { type: "terminal_opened"; command: string }
  | { type: "finished" };

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
  objects: KnObject[];
  notes: KnNote[];
}

export type KnowledgeEvent =
  | { type: "progress"; connection_id: string; schema: string; done: number; total: number }
  | {
      type: "finished";
      connection_id: string;
      report: { schemas: number; objects: number; changed: number; removed: number; errors: string[]; cancelled: boolean };
    }
  | { type: "cancelled"; connection_id: string }
  | { type: "failed"; connection_id: string; error: string };

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
