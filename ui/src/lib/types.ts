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
import type { TableColumns } from "./bindings/TableColumns";
import type { ObjectKind } from "./bindings/ObjectKind";
import type { SchemaInfo } from "./bindings/SchemaInfo";
import type { TableLayout } from "./bindings/TableLayout";
import type { CostEstimate } from "./bindings/CostEstimate";
import type { ParamValue } from "./bindings/ParamValue";
import type { StatementKind } from "./bindings/StatementKind";
import type { ColumnMeta } from "./bindings/ColumnMeta";
import type { TypeFamily } from "./bindings/TypeFamily";
import type { ResultInfo } from "./bindings/ResultInfo";
import type { Page } from "./bindings/Page";
import type { FindResult } from "./bindings/FindResult";
import type { ColumnStats } from "./bindings/ColumnStats";
import type { SelectionSummary } from "./bindings/SelectionSummary";
import type { FilterOp } from "./bindings/FilterOp";
import type { ColumnFilter } from "./bindings/ColumnFilter";
import type { SortKey } from "./bindings/SortKey";
import type { ViewSpec } from "./bindings/ViewSpec";
import type { ChartSpec } from "./bindings/ChartSpec";
import type { ChartData } from "./bindings/ChartData";
import type { RunRequest } from "./bindings/RunRequest";
import type { RunResponse } from "./bindings/RunResponse";
import type { PlannedStatement } from "./bindings/PlannedStatement";
import type { EngineError } from "./bindings/EngineError";
import type { JobEvent } from "./bindings/JobEvent";
import type { TestResult } from "./bindings/TestResult";
import type { OutputInfo } from "./bindings/OutputInfo";
import type { CompareKind } from "./bindings/CompareKind";
import type { CompareOptions } from "./bindings/CompareOptions";
import type { ComparePair } from "./bindings/ComparePair";
import type { CompareRun } from "./bindings/CompareRun";
import type { CompareSpec } from "./bindings/CompareSpec";
import type { CompareSource } from "./bindings/CompareSource";
import type { CompareSummary } from "./bindings/CompareSummary";
import type { ColumnChanges } from "./bindings/ColumnChanges";
import type { OutputState } from "./bindings/OutputState";
import type { RunStatus } from "./bindings/RunStatus";
import type { EditTarget } from "./bindings/EditTarget";
import type { ResultSource } from "./bindings/ResultSource";
import type { EditStatement } from "./bindings/EditStatement";
import type { ApplyReport } from "./bindings/ApplyReport";
import type { ResultLink } from "./bindings/ResultLink";
import type { LoadCheck } from "./bindings/LoadCheck";
import type { LoadDryRun } from "./bindings/LoadDryRun";
import type { LoadReport } from "./bindings/LoadReport";
import type { ParamSet } from "./bindings/ParamSet";
import type { RunParams } from "./bindings/RunParams";
import type { LoadTarget } from "./bindings/LoadTarget";
import type { PlanNode } from "./bindings/PlanNode";
import type { QueryPlan } from "./bindings/QueryPlan";
import type { DiffStatus } from "./bindings/DiffStatus";
import type { ColumnChange } from "./bindings/ColumnChange";
import type { ObjectDiff } from "./bindings/ObjectDiff";
import type { SchemaDiff } from "./bindings/SchemaDiff";
import type { ExportFormat } from "./bindings/ExportFormat";
import type { ExportOptions } from "./bindings/ExportOptions";
import type { BatchLimits } from "./bindings/BatchLimits";
import type { SavedQuery } from "./bindings/SavedQuery";
import type { HistoryEntry } from "./bindings/HistoryEntry";
import type { HistoryQuery } from "./bindings/HistoryQuery";
import type { HistoryClear } from "./bindings/HistoryClear";
import type { TabState } from "./bindings/TabState";
import type { Folder } from "./bindings/Folder";
import type { FolderKind } from "./bindings/FolderKind";
import type { CellKind } from "./bindings/CellKind";
import type { CellRunSummary } from "./bindings/CellRunSummary";
import type { NotebookCell } from "./bindings/NotebookCell";
import type { Notebook } from "./bindings/Notebook";
import type { NotebookSummary } from "./bindings/NotebookSummary";
import type { ExplorerCacheState } from "./bindings/ExplorerCacheState";
import type { CachedListing } from "./bindings/CachedListing";
import type { CachedColumns } from "./bindings/CachedColumns";
import type { KnObject } from "./bindings/KnObject";
import type { KnNote } from "./bindings/KnNote";
import type { NoteEntry } from "./bindings/NoteEntry";
import type { ImportItem } from "./bindings/ImportItem";
import type { ImportAction } from "./bindings/ImportAction";
import type { KnState } from "./bindings/KnState";
import type { AiSessionRecord } from "./bindings/AiSessionRecord";
import type { AuditEntry } from "./bindings/AuditEntry";
import type { AuthStatus } from "./bindings/AuthStatus";
import type { DeviceCodePrompt } from "./bindings/DeviceCodePrompt";
import type { TerminalOption } from "./bindings/TerminalOption";
import type { TerminalSetting } from "./bindings/TerminalSetting";
import type { MigrationReport } from "./bindings/MigrationReport";
import type { AgentEvent } from "./bindings/AgentEvent";
import type { AiSendArgs } from "./bindings/AiSendArgs";
import type { AiError } from "./bindings/AiError";
import type { Budget } from "./bindings/Budget";
import type { BudgetMeter } from "./bindings/BudgetMeter";
import type { ModelInfo } from "./bindings/ModelInfo";
import type { KiroStatus } from "./bindings/KiroStatus";
import type { ProviderAuth } from "./bindings/ProviderAuth";
import type { ProviderConfig } from "./bindings/ProviderConfig";
import type { ProviderKind } from "./bindings/ProviderKind";
import type { UiContext } from "./bindings/UiContext";
import type { IndexPlan } from "./bindings/IndexPlan";
import type { JoinStatus } from "./bindings/JoinStatus";
import type { LearnedJoin } from "./bindings/LearnedJoin";
import type { LearnedJoinView } from "./bindings/LearnedJoinView";
import type { LearnedView } from "./bindings/LearnedView";
import type { LearnReport } from "./bindings/LearnReport";
import type { LearnState } from "./bindings/LearnState";
import type { TableUsage } from "./bindings/TableUsage";
import type { PlanSchema } from "./bindings/PlanSchema";
import type { LineageRange } from "./bindings/LineageRange";
import type { LineageNodeKind } from "./bindings/LineageNodeKind";
import type { LineageEdgeKind } from "./bindings/LineageEdgeKind";
import type { LineageColumn } from "./bindings/LineageColumn";
import type { LineageNode } from "./bindings/LineageNode";
import type { LineageEdge } from "./bindings/LineageEdge";
import type { LineageFlow } from "./bindings/LineageFlow";
import type { LineageGraph } from "./bindings/LineageGraph";
import type { BackgroundStatus } from "./bindings/BackgroundStatus";
import type { CachedExplorer } from "./bindings/CachedExplorer";
import type { ConnectionView } from "./bindings/ConnectionView";
import type { CredentialStoreView } from "./bindings/CredentialStoreView";
import type { ExcelSheet } from "./bindings/ExcelSheet";
import type { ExplorerRevalidation } from "./bindings/ExplorerRevalidation";
import type { SchemaRefresh } from "./bindings/SchemaRefresh";
import type { LineageCheck } from "./bindings/LineageCheck";
import type { LineageView } from "./bindings/LineageView";
import type { LoadResultArgs } from "./bindings/LoadResultArgs";
import type { TestDataAi } from "./bindings/TestDataAi";
import type { TestDataColumn } from "./bindings/TestDataColumn";
import type { TestDataForeignKey } from "./bindings/TestDataForeignKey";
import type { TestDataOutput } from "./bindings/TestDataOutput";
import type { TestDataPlan } from "./bindings/TestDataPlan";
import type { TestDataSpec } from "./bindings/TestDataSpec";
import type { Span } from "./bindings/Span";
import type { ImportCandidate } from "./bindings/ImportCandidate";
import type { ImportPreview } from "./bindings/ImportPreview";
import type { ImportReport } from "./bindings/ImportReport";
import type { ImportSource } from "./bindings/ImportSource";
import type { JobLogEvent } from "./bindings/JobLogEvent";
import type { JobRunEvent } from "./bindings/JobRunEvent";
import type { KnObjectPage } from "./bindings/KnObjectPage";
import type { KnowledgeView } from "./bindings/KnowledgeView";
import type { NotesImportPreview } from "./bindings/NotesImportPreview";
import type { ProviderUsage } from "./bindings/ProviderUsage";
import type { ProviderView } from "./bindings/ProviderView";
import type { SqlFile } from "./bindings/SqlFile";
import type { UpdateInfo } from "./bindings/UpdateInfo";
/** What `ai_send` takes: the agent request and the tab it is for. */
export type AgentRequest = AiSendArgs;
import type { GhostRequest } from "./bindings/GhostRequest";
export type { GhostRequest };
import type { AiProviderRecord } from "./bindings/AiProviderRecord";
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
  TableColumns,
  ObjectKind,
  SchemaInfo,
  TableLayout,
  CostEstimate,
  ParamValue,
  StatementKind,
  ColumnMeta,
  TypeFamily,
  ResultInfo,
  Page,
  FindResult,
  ColumnStats,
  SelectionSummary,
  FilterOp,
  ColumnFilter,
  SortKey,
  ViewSpec,
  ChartSpec,
  ChartData,
  RunRequest,
  RunResponse,
  PlannedStatement,
  EngineError,
  JobEvent,
  TestResult,
  OutputInfo,
  CompareKind,
  CompareOptions,
  ComparePair,
  CompareRun,
  CompareSpec,
  CompareSource,
  CompareSummary,
  ColumnChanges,
  OutputState,
  RunStatus,
  EditTarget,
  ResultSource,
  EditStatement,
  ApplyReport,
  ResultLink,
  LoadCheck,
  LoadDryRun,
  LoadReport,
  ParamSet,
  RunParams,
  LoadTarget,
  PlanNode,
  QueryPlan,
  DiffStatus,
  ColumnChange,
  ObjectDiff,
  SchemaDiff,
  ExportFormat,
  ExportOptions,
  BatchLimits,
  SavedQuery,
  HistoryEntry,
  HistoryQuery,
  HistoryClear,
  TabState,
  Folder,
  FolderKind,
  CellKind,
  CellRunSummary,
  NotebookCell,
  Notebook,
  NotebookSummary,
  ExplorerCacheState,
  CachedListing,
  CachedColumns,
  KnObject,
  KnNote,
  NoteEntry,
  ImportItem,
  ImportAction,
  KnState,
  AiSessionRecord,
  AuditEntry,
  AuthStatus,
  DeviceCodePrompt,
  TerminalOption,
  TerminalSetting,
  MigrationReport,
  AgentEvent,
  AiError,
  Budget,
  BudgetMeter,
  ModelInfo,
  KiroStatus,
  ProviderAuth,
  ProviderConfig,
  ProviderKind,
  UiContext,
  IndexPlan,
  JoinStatus,
  LearnedJoin,
  LearnedJoinView,
  LearnedView,
  LearnReport,
  LearnState,
  TableUsage,
  PlanSchema,
  LineageRange,
  LineageNodeKind,
  LineageEdgeKind,
  LineageColumn,
  LineageNode,
  LineageEdge,
  LineageFlow,
  LineageGraph,
  BackgroundStatus,
  CachedExplorer,
  ConnectionView,
  CredentialStoreView,
  ExcelSheet,
  ExplorerRevalidation,
  SchemaRefresh,
  LineageCheck,
  LineageView,
  LoadResultArgs,
  TestDataAi,
  TestDataColumn,
  TestDataForeignKey,
  TestDataOutput,
  TestDataPlan,
  TestDataSpec,
  Span,
  ImportCandidate,
  ImportPreview,
  ImportReport,
  ImportSource,
  JobLogEvent,
  JobRunEvent,
  KnObjectPage,
  KnowledgeView,
  NotesImportPreview,
  ProviderUsage,
  ProviderView,
  SqlFile,
  UpdateInfo,
  AiProviderRecord,
};


export interface AiMessageRecord {
  id: number;
  session_id: string;
  role: "user" | "assistant" | "tool";
  content: { text?: string; tool_calls?: { id: string; name: string; arguments: unknown }[]; tool_name?: string } | string;
  tokens_in?: number | null;
  tokens_out?: number | null;
  created_at: number;
}

/** A `:name` parameter in the editor (UTF-16 offsets of `:name`). */
export interface ParamSpan {
  name: string;
  from: number;
  to: number;
  /** Oracle `&name` / `&&name`: replaced by the value as typed. */
  textual?: boolean;
}

































// ---- lineage





























export type ChartAgg = import("./bindings/Agg").Agg;














// ---- notebooks






// ---- AI








export type AiMode = import("./bindings/Mode").Mode;






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





export type AuthEvent =
  | { type: "browser_opened"; url: string }
  | { type: "device_code"; prompt: DeviceCodePrompt }
  | { type: "terminal_opened"; command: string; terminal?: string }
  /** No terminal could be opened (or the user runs it): show the command. */
  | { type: "run_command"; command: string; reason?: string | null }
  | { type: "finished" };



export type TerminalLaunch = { type: "opened"; terminal: string } | { type: "manual"; command: string; reason?: string | null };










export type KnowledgeEvent =
  | { type: "progress"; connection_id: string; schema: string; done: number; total: number }
  | {
      type: "finished";
      connection_id: string;
      report: { schemas: number; objects: number; changed: number; removed: number; errors: string[]; cancelled: boolean; skipped?: number };
    }
  | { type: "cancelled"; connection_id: string }
  | { type: "failed"; connection_id: string; error: string }
  /** Joins or table usage learned from query history (no toast). */
  | { type: "learned"; connection_id: string; report: LearnReport };

export interface OracleClientStatus {
  installed: boolean;
  version: string | null;
  lib_dir: string | null;
  /** Found on the computer but not loaded: the user chooses it. */
  found: string | null;
  message: string | null;
  platform: { os: string; arch: string; download_page: string; download_url: string | null; auto_install: boolean; note: string | null };
}

export type OracleInstallProgress =
  | { phase: "downloading"; received: number; total: number | null }
  | { phase: "installing" }
  | { phase: "verifying" };

export type CredentialStoreKind = import("./bindings/StoreKind").StoreKind;



// ------------------------------------------------------------------ jobs

export type NodeRunStatus = Exclude<NodeRunSummary["status"], "">;










/** Dry run of a load step: nothing is kept. */
















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


/** Thin Oracle driver: installed or not, and what to download by hand. */
export interface OracleAgentInfo {
  installed: string | null;
  file_name: string;
  url: string;
  sha256: string | null;
  release: string;
}












