import { useEffect, useRef, useState } from "react";
import {
  AlertCircle,
  ArrowDown,
  ArrowUp,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Code2,
  Loader2,
  Play,
  PlayCircle,
  Sparkles,
  Trash2,
  Type,
} from "lucide-react";
import type { CellKind, NotebookCell } from "../../lib/types";
import { formatCount, formatDuration } from "../../lib/util";
import { useStore } from "../../store";
import { registerKeyConnection } from "../../aiStore";
import { ResizeHandle } from "../ResizeHandle";
import { OUTPUT_DEFAULT, OUTPUT_MAX, OUTPUT_MIN } from "../../lib/resize";
import { ResultsPanel } from "../ResultsPanel";
import { OutputChip } from "../OutputChip";
import type { CellDeps } from "../../lib/dataflow";
import { kbd } from "../../lib/platform";
import { CellEditor } from "./CellEditor";
import { MarkdownCell } from "./MarkdownCell";
import { CellAi } from "./CellAi";
import { cellKey } from "./shared";

// ------------------------------------------------------------------ cell

interface CellProps {
  nbId: string;
  cell: NotebookCell;
  index: number;
  total: number;
  connectionId: string | null;
  notebookConnection: string | null;
  focused: boolean;
  onFocus: () => void;
  onChange: (patch: Partial<NotebookCell>) => void;
  onRun: () => void;
  onRunAdvance: () => void;
  onRunFromHere: () => void;
  onDelete: () => void;
  onMove: (d: -1 | 1) => void;
  onAddBelow: (k: CellKind) => void;
  deps?: CellDeps;
  cellIndexOf: (id: string) => number;
  onRunDependents: () => void;
}

export function Cell(props: CellProps) {
  const {
    nbId,
    cell,
    index,
    total,
    focused,
    onFocus,
    onChange,
    onDelete,
    onMove,
    onAddBelow,
  } = props;
  const key = cellKey(nbId, cell.id);
  const run = useStore((s) => s.runs[key]);
  const connections = useStore((s) => s.connections);
  const [aiOpen, setAiOpen] = useState(false);
  const outRef = useRef<HTMLDivElement>(null);
  const [outH, setOutH] = useState<number | null>(null);
  const conn = connections.find((c) => c.id === props.connectionId);

  useEffect(
    () => registerKeyConnection(key, props.connectionId),
    [key, props.connectionId],
  );

  const status = run?.running ? "running" : run?.finishedStatus;
  const deps = props.deps;
  const lastOutput = run?.statements
    .slice()
    .reverse()
    .find((x) => x.output)?.output;
  // Reads outputs: runs on Results (chosen automatically, or because the
  // chosen connection is not DuckDB).
  const autoResults = cell.kind === "sql" && !cell.connection_id && (deps?.reads.length ?? 0) > 0 && props.connectionId !== props.notebookConnection;
  const viaResults = cell.kind === "sql" && (deps?.reads.length ?? 0) > 0 && (autoResults || conn?.config.kind !== "duckdb");
  return (
    <div
      className={`group relative rounded-xl border bg-panel transition-colors ${focused ? "border-accent/60 shadow-[0_0_0_1px_color-mix(in_srgb,var(--accent)_25%,transparent)]" : "border-line"}`}
      onMouseDown={onFocus}
    >
      <div className="flex items-center gap-1 px-2 pt-1.5">
        <span className="w-8 text-center font-mono text-[10.5px] text-muted">
          [{index + 1}]
        </span>
        {cell.kind === "sql" ? (
          <button
            className={`icon-btn h-6 w-6 ${run?.running ? "text-accent" : ""}`}
            aria-label={run?.running ? "Stop cell" : "Run cell"}
            title={
              run?.running ? "Stop" : kbd("Run cell (⌘↵) · run and advance (⇧↵)")
            }
            onClick={() =>
              run?.running
                ? void useStore.getState().cancelTab(key)
                : props.onRun()
            }
          >
            {run?.running ? (
              <Loader2 size={13} className="animate-spin" />
            ) : (
              <Play size={13} fill="currentColor" />
            )}
          </button>
        ) : (
          <span className="flex h-6 w-6 items-center justify-center text-muted">
            <Type size={12} />
          </span>
        )}
        <CellStatus cell={cell} status={status} />
        {cell.kind === "sql" && (
          <OutputNameField
            value={cell.output_name ?? ""}
            onChange={(v) => onChange({ output_name: v || null })}
          />
        )}
        {lastOutput && <OutputChip output={lastOutput} compact />}
        {deps?.stale && (
          <button
            className="flex items-center gap-1 rounded bg-warning/15 px-1.5 py-0.5 text-[10.5px] text-warning"
            title={`Inputs changed since this cell ran (cells ${deps.producers.map(props.cellIndexOf).join(", ")}). Click to re-run.`}
            onClick={props.onRun}
          >
            stale — run
          </button>
        )}
        {deps && deps.missing.length > 0 && (
          <span
            className="rounded bg-danger/12 px-1.5 py-0.5 text-[10.5px] text-danger"
            title="No earlier cell names this output and no such output exists"
          >
            missing: {deps.missing.map((m) => `results.${m}`).join(", ")}
          </span>
        )}
        {viaResults && (
          <span
            className="rounded bg-panel-2 px-1.5 py-0.5 text-[10.5px] text-muted"
            title="This cell reads outputs, so it runs locally on the Results (DuckDB) connection"
          >
            runs on Results
          </span>
        )}
        {deps && deps.dependents.length > 0 && status !== "running" && (
          <button
            className="rounded px-1.5 py-0.5 text-[10.5px] text-muted hover:bg-hover hover:text-fg"
            title={`Cells ${deps.dependents.map(props.cellIndexOf).join(", ")} read this cell's output`}
            onClick={props.onRunDependents}
          >
            → {deps.dependents.length} dependent
            {deps.dependents.length === 1 ? "" : "s"} · run
          </button>
        )}
        <div className="ml-auto flex items-center gap-0.5 opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100">
          {cell.kind === "sql" && (
            <select
              className="h-6 max-w-[140px] rounded border border-line bg-panel-2 px-1 text-[11px] outline-none"
              aria-label="Cell connection"
              title="Run this cell on a different connection"
              value={cell.connection_id ?? ""}
              onChange={(e) =>
                onChange({ connection_id: e.target.value || null })
              }
            >
              <option value="">{autoResults ? "Results (reads outputs)" : "Notebook connection"}</option>
              {connections.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </select>
          )}
          <button
            className="icon-btn h-6 w-6"
            title="Ask AI about this cell"
            aria-label="Ask AI"
            onClick={() => setAiOpen(!aiOpen)}
          >
            <Sparkles size={12} />
          </button>
          <button
            className="icon-btn h-6 w-6"
            title={cell.kind === "sql" ? "Convert to text" : "Convert to SQL"}
            aria-label="Convert cell type"
            onClick={() =>
              onChange({ kind: cell.kind === "sql" ? "markdown" : "sql" })
            }
          >
            {cell.kind === "sql" ? <Type size={12} /> : <Code2 size={12} />}
          </button>
          {cell.kind === "sql" && (
            <button
              className="icon-btn h-6 w-6"
              title="Run from here"
              aria-label="Run from here"
              onClick={props.onRunFromHere}
            >
              <PlayCircle size={12} />
            </button>
          )}
          <button
            className="icon-btn h-6 w-6"
            aria-label="Move up"
            disabled={index === 0}
            onClick={() => onMove(-1)}
          >
            <ArrowUp size={12} />
          </button>
          <button
            className="icon-btn h-6 w-6"
            aria-label="Move down"
            disabled={index === total - 1}
            onClick={() => onMove(1)}
          >
            <ArrowDown size={12} />
          </button>
          <button
            className="icon-btn h-6 w-6 hover:text-danger"
            aria-label="Delete cell"
            onClick={onDelete}
          >
            <Trash2 size={12} />
          </button>
        </div>
      </div>

      <div className="px-2 pb-2 pl-11">
        {cell.kind === "sql" ? (
          <CellEditor
            editorKey={key}
            source={cell.source}
            connectionId={props.connectionId}
            error={run?.errorRange}
            height={cell.editor_height ?? null}
            onResize={(h) => onChange({ editor_height: h })}
            onChange={(source) => onChange({ source })}
            onRun={props.onRun}
            onRunAdvance={props.onRunAdvance}
            onFocus={onFocus}
          />
        ) : (
          <MarkdownCell
            source={cell.source}
            onChange={(source) => onChange({ source })}
            autoEdit={!cell.source}
          />
        )}
        {aiOpen && (
          <CellAi
            cell={cell}
            editorKey={key}
            connectionId={props.connectionId ?? props.notebookConnection}
            hasError={run?.finishedStatus === "error"}
            onClose={() => setAiOpen(false)}
          />
        )}
      </div>

      {cell.kind === "sql" && run && (
        <div className="border-t border-line">
          <button
            className="flex w-full items-center gap-1 px-3 py-1 text-left text-[11px] text-muted hover:text-fg"
            onClick={() => onChange({ collapsed: !cell.collapsed })}
            aria-expanded={!cell.collapsed}
          >
            {cell.collapsed ? (
              <ChevronRight size={12} />
            ) : (
              <ChevronDown size={12} />
            )}
            Output{conn ? ` · ${conn.name}` : ""}
          </button>
          {!cell.collapsed && (
            <>
              <div
                ref={outRef}
                className="overflow-hidden"
                style={{ height: outH ?? cell.output_height ?? OUTPUT_DEFAULT }}
              >
                <ResultsPanel
                  tabId={key}
                  connectionId={props.connectionId}
                  title={`cell_${index + 1}`}
                  compact
                />
              </div>
              <ResizeHandle
                label="Output height"
                min={OUTPUT_MIN}
                max={OUTPUT_MAX}
                height={() =>
                  outRef.current?.getBoundingClientRect().height ??
                  OUTPUT_DEFAULT
                }
                onChange={setOutH}
                onCommit={(h) => {
                  setOutH(null);
                  onChange({ output_height: h });
                }}
                onReset={() => {
                  setOutH(null);
                  onChange({ output_height: null });
                }}
              />
            </>
          )}
        </div>
      )}

      <div className="absolute -bottom-3 left-1/2 z-10 hidden -translate-x-1/2 gap-1 group-hover:flex">
        <button
          className="rounded-full border border-line bg-panel px-2 py-0.5 text-[10.5px] text-muted shadow hover:text-fg"
          onClick={() => onAddBelow("sql")}
        >
          + SQL
        </button>
        <button
          className="rounded-full border border-line bg-panel px-2 py-0.5 text-[10.5px] text-muted shadow hover:text-fg"
          onClick={() => onAddBelow("markdown")}
        >
          + Text
        </button>
      </div>
    </div>
  );
}

function CellStatus({ cell, status }: { cell: NotebookCell; status?: string }) {
  if (cell.kind !== "sql") return null;
  if (status === "running")
    return <span className="text-[11px] text-accent">Running…</span>;
  const lr = cell.last_run;
  if (!lr) return null;
  return (
    <span
      className="flex items-center gap-1 text-[11px] text-muted"
      title={lr.error ?? undefined}
    >
      {lr.error ? (
        <AlertCircle size={11} className="text-danger" />
      ) : (
        <CheckCircle2 size={11} className="text-success" />
      )}
      {lr.rows !== null && lr.rows !== undefined
        ? `${formatCount(lr.rows)} rows · `
        : ""}
      {formatDuration(lr.duration_ms)}
    </span>
  );
}

function OutputNameField({
  value,
  onChange,
}: {
  value: string;
  onChange: (v: string) => void;
}) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  const valid =
    !draft ||
    (/^[A-Za-z_][A-Za-z0-9_]*$/.test(draft) &&
      !draft.includes("__") &&
      !/^r\d+$/i.test(draft));
  return (
    <label
      className="flex items-center gap-1 text-[10.5px] text-muted"
      title="Name this cell's output; later cells can query results.<name>"
    >
      →
      <input
        className={`h-5 w-24 rounded border bg-transparent px-1 font-mono text-[11px] text-fg outline-none focus:bg-panel-2 ${valid ? "border-transparent hover:border-line focus:border-accent" : "border-danger"}`}
        placeholder="name output"
        aria-label="Output name"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => (valid ? onChange(draft.trim()) : setDraft(value))}
        onKeyDown={(e) =>
          e.key === "Enter" && (e.target as HTMLInputElement).blur()
        }
      />
    </label>
  );
}
