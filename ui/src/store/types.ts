import type { AuthEvent, CompareSpec, EngineError, ObjectKind, OutputInfo, ParamValue, PlannedStatement, ResultInfo, RunStatus, TabState } from "../lib/types";
import type { RunTips } from "../queryTips";
import type { AppSlice } from "./appSlice";
import type { ConnectionsSlice } from "./connectionsSlice";
import type { ExplorerSlice } from "./explorerSlice";
import type { LibrarySlice } from "./librarySlice";
import type { ParamsSlice } from "./paramsSlice";
import type { RunsSlice } from "./runsSlice";
import type { TabsSlice } from "./tabsSlice";
import type { UiSlice } from "./uiSlice";

export interface Tab extends TabState {
  /** Unsaved changes relative to the linked saved query. */
  dirty?: boolean;
  /** File tabs: the text and modification time on disk when last read or saved. */
  file_text?: string;
  file_mtime?: number;
  /** Read-only DDL view of an object (explorer "Show DDL"); not saved across restarts. */
  ddl?: DdlRef | null;
  /** Lineage diagram of SQL from an editor (`sql` holds the analysed text); not saved across restarts. */
  lineage?: LineageRef | null;
  /** Job editor (the job is saved in the workspace); not saved across restarts. */
  job_id?: string | null;
  /** Query plan of `sql` (EXPLAIN); not saved across restarts. */
  plan?: PlanRef | null;
  /** Schema compare; not saved across restarts. */
  compare?: CompareRef | null;
  /** Compare of two outputs (the data); not saved across restarts. */
  diff?: DiffRef | null;
  /** ER diagram of a schema (on the tab's connection); not saved across restarts. */
  er?: ErRef | null;
  /** Dashboard of pinned charts (the dashboard is saved in settings). */
  dashboard_id?: string | null;
}

/** The schema an ER diagram tab shows (`schema` is the explorer id, `label` for display). */
export interface ErRef {
  schema: string;
  label: string;
}

/** The two schemas a compare tab shows (the script changes `target`). */
export interface CompareRef {
  source: { connection_id: string; schema: string } | null;
  target: { connection_id: string; schema: string } | null;
}

/** What an output compare tab compares (outputs, keys, columns, options). */
export interface DiffRef {
  spec: CompareSpec;
}

/** Where a plan tab's statement came from. */
export interface PlanRef {
  /** Query tab whose session runs the EXPLAIN (temp tables, open transaction). */
  source: string;
  analyze: boolean;
  /** `:name` values the statement uses. */
  params?: Record<string, ParamValue>;
}

/** Where a lineage tab's SQL came from. */
export interface LineageRef {
  /** Editor key (query tab id or notebook cell key). */
  source: string;
  /** Offset of the analysed text in the source editor (a selection). */
  base: number;
  /** The whole editor (Refresh re-reads it) rather than a selection. */
  whole: boolean;
  /** A tab of one column's flow (node by name, as node ids follow the analysis). */
  column?: { node: string; column?: string } | null;
}

/** The object a DDL tab shows (on the tab's connection). */
export interface DdlRef {
  schema: string;
  name: string;
  kind: ObjectKind;
}

/** Tabs that hold an editable SQL editor (not notebooks, outputs or DDL views). */
export const isQueryTab = (t: Tab | undefined): t is Tab => !!t && !t.notebook_id && !t.output_ref && !t.ddl && !t.lineage && !t.job_id && !t.plan && !t.compare && !t.diff && !t.er && !t.dashboard_id;

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
  /** The server has finished it; the result is being downloaded. */
  downloading?: boolean;
  /** Time it took on the server (excluding the download), when known. */
  serverMs?: number;
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

export type SidebarPanel = "connections" | "saved" | "notebooks" | "jobs" | "outputs" | "history";
export type Theme = "dark" | "light";

export interface RunInput {
  doc: string;
  selFrom: number;
  selTo: number;
  cursor: number;
}

export interface SignInState {
  connectionId: string | null;
  label: string;
  event: AuthEvent | null;
  /**
   * Opened by the backend's own sign-in (connecting from the explorer, a
   * query, completion…), not by a Sign in button that closes it when its
   * call returns: closed as soon as the browser hands the sign-in back.
   */
  implicit?: boolean;
}

export interface ExplorerStatus {
  /** Showing the cached tree; `offline` = the server could not be reached. */
  offline?: string | null;
  /** When the cache was last checked against the server. */
  checkedAt?: number | null;
  /** The engine has schema fingerprints (otherwise schemas are re-listed when opened). */
  fingerprints?: boolean;
  /** Listed schemas not checked yet (refreshed when opened). */
  deferred?: string[];
  /** Background check running. */
  checking?: boolean;
}

/** The whole store: every slice's state and actions. */
export type State = AppSlice & UiSlice & TabsSlice & RunsSlice & ConnectionsSlice & ExplorerSlice & LibrarySlice & ParamsSlice;
