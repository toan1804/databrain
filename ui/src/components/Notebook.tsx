// Notebook tab: ordered SQL / Markdown cells bound to a connection.
//
// Every SQL cell runs under its own run key (`nb:{notebook}:{cell}`) so it
// keeps its own results, while all cells share one database session
// (`nb:{notebook}`), so temp tables and session settings carry across cells.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  closeBrackets,
  closeBracketsKeymap,
  completionKeymap,
} from "@codemirror/autocomplete";
import {
  defaultKeymap,
  history,
  historyKeymap,
  indentWithTab,
} from "@codemirror/commands";
import {
  bracketMatching,
  indentOnInput,
  syntaxHighlighting,
} from "@codemirror/language";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { Compartment, EditorState } from "@codemirror/state";
import {
  EditorView,
  drawSelection,
  keymap,
  placeholder,
} from "@codemirror/view";
import {
  AlertCircle,
  ArrowDown,
  ArrowUp,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Code2,
  FileText,
  Loader2,
  Play,
  PlayCircle,
  Plus,
  Sparkles,
  Square,
  Trash2,
  Type,
  Wand2,
} from "lucide-react";
import { api, toError } from "../lib/api";
import type {
  CellKind,
  Notebook as NotebookT,
  NotebookCell,
} from "../lib/types";
import { formatCount, formatDuration, uid } from "../lib/util";
import { useStore } from "../store";
import { registerKeyConnection, useAi } from "../aiStore";
import { editorBridge } from "../editorBridge";
import { errorField, highlight, langExtension, setError } from "./SqlEditor";
import { canFetchMetadata, sqlAssist } from "./sqlAssist";
import { queryHints } from "./queryHintsExt";
import { ResizeHandle } from "./ResizeHandle";
import {
  EDITOR_MAX,
  EDITOR_MIN,
  OUTPUT_DEFAULT,
  OUTPUT_MAX,
  OUTPUT_MIN,
} from "../lib/resize";
import { ResultsPanel } from "./ResultsPanel";
import { Markdown } from "./Markdown";
import { ConnDot, EnvBadge } from "./ui";
import { OutputChip, OutputQueryTarget } from "./OutputChip";
import { resultsConnection } from "../outputs";
import { cellDeps, referencedOutputs, type CellDeps } from "../lib/dataflow";

const SAVE_DELAY = 700;

export const cellKey = (nbId: string, cellId: string) => `nb:${nbId}:${cellId}`;

/** Wait until the run for `key` is no longer running. */
function waitForRun(key: string): Promise<void> {
  return new Promise((resolve) => {
    const check = () => !useStore.getState().runs[key]?.running;
    if (check()) return resolve();
    const unsub = useStore.subscribe((s) => {
      if (!s.runs[key]?.running) {
        unsub();
        resolve();
      }
    });
  });
}

export function NotebookView({
  tabId,
  notebookId,
  visible,
}: {
  tabId: string;
  notebookId: string;
  visible: boolean;
}) {
  const [nb, setNb] = useState<NotebookT | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState<"idle" | "pending" | "saving">("idle");
  const [focused, setFocused] = useState<string | null>(null);
  const [runningAll, setRunningAll] = useState(false);
  const stopAll = useRef(false);
  const saveTimer = useRef<ReturnType<typeof setTimeout>>();
  const latest = useRef<NotebookT | null>(null);
  const connections = useStore((s) => s.connections);
  const updateTab = useStore((s) => s.updateTab);
  const toast = useStore((s) => s.toast);
  // Subscribed (not read once) so Run all / Stop always reflects the cells' state.
  const anyRunning = useStore((s) =>
    Object.entries(s.runs).some(
      ([k, r]) => r.running && k.startsWith(`nb:${notebookId}:`),
    ),
  );

  useEffect(() => {
    let alive = true;
    api
      .getNotebook(notebookId)
      .then((n) => {
        if (!alive) return;
        setNb(n);
        latest.current = n;
      })
      .catch((e) => alive && setError(toError(e).message));
    return () => {
      alive = false;
    };
  }, [notebookId]);

  const flush = useCallback(async () => {
    clearTimeout(saveTimer.current);
    const cur = latest.current;
    if (!cur) return;
    setSaving("saving");
    try {
      await api.saveNotebook(cur);
      setSaving("idle");
      void useStore.getState().refreshNotebooks();
    } catch (e) {
      setSaving("idle");
      toast(`Notebook not saved: ${toError(e).message}`, "error");
    }
  }, [toast]);

  // Tab closed: save, then release the shared session and cell results.
  useEffect(
    () => () => {
      void flush();
      void api.closeTab(`nb:${notebookId}`).catch(() => {});
      for (const c of latest.current?.cells ?? [])
        void api.closeTab(cellKey(notebookId, c.id)).catch(() => {});
    },
    [flush, notebookId],
  );

  const update = useCallback(
    (fn: (n: NotebookT) => NotebookT) => {
      setNb((prev) => {
        if (!prev) return prev;
        const next = fn(prev);
        latest.current = next;
        return next;
      });
      setSaving("pending");
      clearTimeout(saveTimer.current);
      saveTimer.current = setTimeout(() => void flush(), SAVE_DELAY);
    },
    [flush],
  );

  const updateCell = useCallback(
    (id: string, patch: Partial<NotebookCell>) =>
      update((n) => ({
        ...n,
        cells: n.cells.map((c) => (c.id === id ? { ...c, ...patch } : c)),
      })),
    [update],
  );

  // Record last-run summaries when cell jobs finish.
  useEffect(() => {
    if (!nb) return;
    return useStore.subscribe((s, prev) => {
      for (const c of latest.current?.cells ?? []) {
        const k = cellKey(notebookId, c.id);
        const r = s.runs[k];
        const p = prev.runs[k];
        if (r && p?.running && !r.running) {
          const failed = r.statements.find((x) => x.status === "error");
          const last = [...r.statements].reverse().find((x) => x.result);
          updateCell(c.id, {
            last_run: {
              finished_at: Date.now(),
              duration_ms: r.durationMs ?? 0,
              rows: last?.result?.total_rows ?? null,
              error: failed?.error?.message ?? null,
            },
          });
        }
      }
    });
  }, [nb === null, notebookId, updateCell]); // eslint-disable-line react-hooks/exhaustive-deps

  const connId = nb?.connection_id ?? null;
  const conn = connections.find((c) => c.id === connId);
  const outputNames = useStore((s) =>
    s.outputs
      .filter((o) => o.state !== "evicted")
      .map((o) => o.name ?? o.handle)
      .join("\u0000"),
  );
  const deps = useMemo(
    () =>
      nb
        ? cellDeps(nb.cells, outputNames ? outputNames.split("\u0000") : [])
        : {},
    [nb, outputNames],
  );
  const runDependents = async (id: string) => {
    const order = latest.current?.cells ?? [];
    const seen = new Set<string>();
    const walk = (x: string) => {
      for (const d of deps[x]?.dependents ?? [])
        if (!seen.has(d)) (seen.add(d), walk(d));
    };
    walk(id);
    for (const c of order) {
      if (!seen.has(c.id)) continue;
      const ok = await runCell(c);
      if (
        !ok ||
        useStore.getState().runs[cellKey(notebookId, c.id)]?.finishedStatus !==
          "success"
      )
        break;
    }
  };

  const cellConn = (c: NotebookCell) => c.connection_id ?? connId;

  const runCell = useCallback(
    async (c: NotebookCell, sqlOverride?: string) => {
      if (c.kind !== "sql") return false;
      const cid = c.connection_id ?? latest.current?.connection_id;
      if (!cid) {
        toast("Choose a connection for this notebook first", "error");
        return false;
      }
      const sql =
        sqlOverride ??
        editorBridge.get(cellKey(notebookId, c.id))?.state.doc.toString() ??
        c.source;
      if (!sql.trim()) return true;
      // Cells that read results.<name> run on the local Results connection
      // unless their connection is DuckDB already.
      let target = cid;
      const kind = useStore.getState().connections.find((x) => x.id === cid)
        ?.config.kind;
      if (referencedOutputs(sql).length > 0 && kind !== "duckdb") {
        try {
          target = await resultsConnection();
        } catch (e) {
          toast(toError(e).message, "error");
          return false;
        }
      }
      const name = c.output_name?.trim() || null;
      const ok = await useStore
        .getState()
        .runSql(
          cellKey(notebookId, c.id),
          target,
          sql,
          0,
          `nb:${notebookId}`,
          name,
        );
      if (ok) await waitForRun(cellKey(notebookId, c.id));
      return ok;
    },
    [notebookId, toast],
  );

  const runAll = async (fromIndex = 0) => {
    if (!latest.current) return;
    setRunningAll(true);
    stopAll.current = false;
    try {
      for (const c of latest.current.cells.slice(fromIndex)) {
        if (stopAll.current) break;
        if (c.kind !== "sql" || !c.source.trim()) continue;
        const ok = await runCell(c);
        const run = useStore.getState().runs[cellKey(notebookId, c.id)];
        if (!ok || run?.finishedStatus !== "success") break; // stop at the first failure
      }
    } finally {
      setRunningAll(false);
    }
  };

  const stop = () => {
    stopAll.current = true;
    for (const c of latest.current?.cells ?? [])
      void useStore.getState().cancelTab(cellKey(notebookId, c.id));
  };

  const addCell = (kind: CellKind, after?: string, source = "") => {
    const cell: NotebookCell = { id: uid(), kind, source };
    update((n) => {
      const i = after
        ? n.cells.findIndex((c) => c.id === after)
        : n.cells.length - 1;
      const cells = n.cells.slice();
      cells.splice(i + 1, 0, cell);
      return { ...n, cells };
    });
    setFocused(cell.id);
    setTimeout(() => editorBridge.focus(cellKey(notebookId, cell.id)), 30);
    return cell;
  };

  const removeCell = (id: string) => {
    void api.closeTab(cellKey(notebookId, id)).catch(() => {});
    update((n) => ({ ...n, cells: n.cells.filter((c) => c.id !== id) }));
  };

  const moveCell = (id: string, d: -1 | 1) =>
    update((n) => {
      const i = n.cells.findIndex((c) => c.id === id);
      const j = i + d;
      if (i < 0 || j < 0 || j >= n.cells.length) return n;
      const cells = n.cells.slice();
      [cells[i], cells[j]] = [cells[j], cells[i]];
      return { ...n, cells };
    });

  /** ⇧↵: run and move to the next cell (adds one at the end). */
  const runAndAdvance = async (c: NotebookCell) => {
    const cells = latest.current?.cells ?? [];
    const i = cells.findIndex((x) => x.id === c.id);
    const next = cells[i + 1];
    if (next) {
      setFocused(next.id);
      editorBridge.focus(cellKey(notebookId, next.id));
    } else addCell("sql", c.id);
    await runCell(c);
  };

  const setConnection = (id: string | null) => {
    void api.closeTab(`nb:${notebookId}`).catch(() => {}); // new connection → new shared session
    update((n) => ({ ...n, connection_id: id }));
  };

  const rename = (name: string) => {
    if (!name.trim()) return;
    update((n) => ({ ...n, name: name.trim() }));
    updateTab(tabId, { title: name.trim() });
  };

  if (error)
    return (
      <div
        className="p-6 text-[13px] text-danger"
        style={{ display: visible ? "block" : "none" }}
      >
        {error}
      </div>
    );
  if (!nb)
    return (
      <div
        className="flex h-full items-center justify-center text-muted"
        style={{ display: visible ? "flex" : "none" }}
      >
        <Loader2 size={18} className="animate-spin" />
      </div>
    );

  return (
    <div
      className="flex h-full min-h-0 flex-col"
      style={{ display: visible ? "flex" : "none" }}
    >
      <div
        className={`flex h-10 shrink-0 items-center gap-2 border-b px-2 ${conn?.env === "prod" ? "border-danger/50 bg-danger/5" : "border-line"}`}
      >
        <FileText size={14} className="text-muted" />
        <input
          className="w-44 rounded bg-transparent px-1 text-[13px] font-medium outline-none hover:bg-hover focus:bg-panel-2"
          defaultValue={nb.name}
          key={nb.id}
          aria-label="Notebook name"
          onBlur={(e) => rename(e.target.value)}
          onKeyDown={(e) =>
            e.key === "Enter" && (e.target as HTMLInputElement).blur()
          }
        />
        <div className="flex items-center gap-1.5 rounded-md border border-line bg-panel-2 pl-2">
          <ConnDot color={conn?.color} connected={conn?.connected} />
          <select
            className="h-7 max-w-[200px] bg-transparent pr-1 text-[12.5px] outline-none"
            aria-label="Notebook connection"
            value={connId ?? ""}
            onChange={(e) => setConnection(e.target.value || null)}
          >
            <option value="">No connection</option>
            {connections.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
        </div>
        {conn && <EnvBadge env={conn.env} />}
        <div className="mx-1 h-5 w-px bg-line" />
        {runningAll || anyRunning ? (
          <button className="btn-danger py-1" onClick={stop}>
            <Square size={12} fill="currentColor" /> Stop
          </button>
        ) : (
          <button
            className="btn-primary py-1"
            onClick={() => void runAll()}
            disabled={!conn}
            title="Run all cells in order (stops at the first error)"
          >
            <PlayCircle size={14} /> Run all
          </button>
        )}
        <button
          className="btn-ghost py-1"
          onClick={() => addCell("sql", focused ?? undefined)}
        >
          <Code2 size={13} /> SQL
        </button>
        <button
          className="btn-ghost py-1"
          onClick={() => addCell("markdown", focused ?? undefined)}
        >
          <Type size={13} /> Text
        </button>
        <span className="ml-auto text-[11.5px] text-muted" aria-live="polite">
          {saving === "saving"
            ? "Saving…"
            : saving === "pending"
              ? "Edited"
              : "Saved"}
        </span>
      </div>

      <div className="min-h-0 flex-1 overflow-auto bg-bg/40 px-4 py-4">
        <div className="mx-auto max-w-[1100px] space-y-3">
          {nb.cells.map((c, i) => (
            // "Query with SQL" on an output in this cell adds a cell below it.
            <OutputQueryTarget.Provider
              key={c.id}
              value={(sql) => void addCell("sql", c.id, sql)}
            >
              <Cell
                key={c.id}
                nbId={notebookId}
                cell={c}
                index={i}
                total={nb.cells.length}
                connectionId={cellConn(c)}
                notebookConnection={connId}
                focused={focused === c.id}
                onFocus={() => setFocused(c.id)}
                onChange={(patch) => updateCell(c.id, patch)}
                onRun={() => void runCell(c)}
                onRunAdvance={() => void runAndAdvance(c)}
                onRunFromHere={() => void runAll(i)}
                onDelete={() => removeCell(c.id)}
                onMove={(d) => moveCell(c.id, d)}
                onAddBelow={(k) => addCell(k, c.id)}
                deps={deps[c.id]}
                cellIndexOf={(id) => nb.cells.findIndex((x) => x.id === id) + 1}
                onRunDependents={() => void runDependents(c.id)}
              />
            </OutputQueryTarget.Provider>
          ))}
          <div className="flex justify-center gap-2 pt-1 pb-10">
            <button
              className="btn-ghost border border-dashed border-line py-1"
              onClick={() => addCell("sql")}
            >
              <Plus size={13} /> SQL cell
            </button>
            <button
              className="btn-ghost border border-dashed border-line py-1"
              onClick={() => addCell("markdown")}
            >
              <Plus size={13} /> Text cell
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

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

function Cell(props: CellProps) {
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
  const viaResults =
    cell.kind === "sql" &&
    (deps?.reads.length ?? 0) > 0 &&
    conn?.config.kind !== "duckdb";
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
              run?.running ? "Stop" : "Run cell (⌘↵) · run and advance (⇧↵)"
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
              <option value="">Notebook connection</option>
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

// ------------------------------------------------------------------ editors

function CellEditor({
  editorKey,
  source,
  connectionId,
  error,
  height,
  onResize,
  onChange,
  onRun,
  onRunAdvance,
  onFocus,
}: {
  editorKey: string;
  source: string;
  connectionId: string | null;
  error?: { from: number; to: number };
  /** Fixed height in px (null = grows with the SQL). */
  height: number | null;
  onResize: (h: number | null) => void;
  onChange: (s: string) => void;
  onRun: () => void;
  onRunAdvance: () => void;
  onFocus: () => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const lang = useRef(new Compartment());
  const cb = useRef({ onChange, onRun, onRunAdvance, onFocus });
  cb.current = { onChange, onRun, onRunAdvance, onFocus };
  const conn = useStore((s) =>
    s.connections.find((c) => c.id === connectionId),
  );
  // Read on each completion request (the cell's connection can change).
  const connRef = useRef(connectionId);
  connRef.current = connectionId;

  useEffect(() => {
    if (!host.current) return;
    const v = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: source,
        extensions: [
          history(),
          drawSelection(),
          indentOnInput(),
          bracketMatching(),
          closeBrackets(),
          sqlAssist(() => connRef.current),
          queryHints(() => connRef.current),
          highlightSelectionMatches(),
          syntaxHighlighting(highlight),
          placeholder("SQL…  ⌘↵ run · ⇧↵ run & next · ⌘I ask AI"),
          errorField,
          lang.current.of(langExtension(undefined)),
          keymap.of([
            {
              key: "Mod-Enter",
              run: () => (cb.current.onRun(), true),
              preventDefault: true,
            },
            {
              key: "Shift-Enter",
              run: () => (cb.current.onRunAdvance(), true),
              preventDefault: true,
            },
            {
              key: "Mod-i",
              run: () => {
                window.dispatchEvent(
                  new CustomEvent("db:inline-ai", {
                    detail: { key: editorKey },
                  }),
                );
                return true;
              },
              preventDefault: true,
            },
            ...closeBracketsKeymap,
            ...defaultKeymap,
            ...searchKeymap,
            ...historyKeymap,
            ...completionKeymap,
            indentWithTab,
          ]),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) cb.current.onChange(u.state.doc.toString());
          }),
          EditorView.domEventHandlers({
            focus: () => {
              editorBridge.setFocused(editorKey);
              cb.current.onFocus();
            },
          }),
          EditorView.contentAttributes.of({ "aria-label": "SQL cell" }),
          EditorView.theme({
            "&": { minHeight: "38px" },
            ".cm-scroller": { fontSize: "13px" },
          }),
        ],
      }),
    });
    view.current = v;
    editorBridge.register(editorKey, v);
    return () => {
      editorBridge.unregister(editorKey, v);
      v.destroy();
      view.current = null;
    };
  }, [editorKey]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    view.current?.dispatch({
      effects: lang.current.reconfigure(langExtension(conn?.config.kind)),
    });
    if (conn && canFetchMetadata(conn))
      useStore
        .getState()
        .loadSchemas(conn.id)
        .catch(() => {});
  }, [conn?.config.kind, conn?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  // External changes (AI edits applied through the bridge already go through the view).
  useEffect(() => {
    const v = view.current;
    if (v && source !== v.state.doc.toString())
      v.dispatch({
        changes: { from: 0, to: v.state.doc.length, insert: source },
      });
  }, [source]);

  useEffect(() => {
    view.current?.dispatch({ effects: setError.of(error ?? null) });
  }, [error]);

  // While dragging the height is local; it is saved on release.
  const [dragH, setDragH] = useState<number | null>(null);
  const h = dragH ?? height;
  useEffect(() => {
    view.current?.requestMeasure();
  }, [h]);
  return (
    <div>
      <div
        ref={host}
        className={`nb-cell overflow-hidden rounded-md border border-line/60 bg-panel-2/40 ${h ? "nb-cell-fixed" : ""}`}
        style={h ? { height: h } : undefined}
      />
      <ResizeHandle
        label="Editor height"
        min={EDITOR_MIN}
        max={EDITOR_MAX}
        height={() =>
          host.current?.getBoundingClientRect().height ?? EDITOR_MIN
        }
        onChange={setDragH}
        onCommit={(v) => {
          setDragH(null);
          onResize(v);
        }}
        onReset={() => {
          setDragH(null);
          onResize(null);
        }}
      />
    </div>
  );
}

function MarkdownCell({
  source,
  onChange,
  autoEdit,
}: {
  source: string;
  onChange: (s: string) => void;
  autoEdit: boolean;
}) {
  const [editing, setEditing] = useState(autoEdit);
  const ref = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    if (editing && ref.current) {
      ref.current.focus();
      ref.current.style.height = "auto";
      ref.current.style.height = `${ref.current.scrollHeight + 2}px`;
    }
  }, [editing]);
  if (editing)
    return (
      <textarea
        ref={ref}
        className="field min-h-[60px] resize-none font-mono text-[12.5px]"
        value={source}
        aria-label="Markdown cell"
        placeholder="Markdown text…  (⌘↵ or Esc to finish)"
        onChange={(e) => {
          onChange(e.target.value);
          e.target.style.height = "auto";
          e.target.style.height = `${e.target.scrollHeight + 2}px`;
        }}
        onBlur={() => setEditing(false)}
        onKeyDown={(e) => {
          if (
            e.key === "Escape" ||
            ((e.metaKey || e.ctrlKey) && e.key === "Enter")
          ) {
            e.preventDefault();
            setEditing(false);
          }
        }}
      />
    );
  return (
    <div
      role="button"
      tabIndex={0}
      className="min-h-[28px] cursor-text rounded-md px-1 py-1 hover:bg-hover/40"
      title="Double-click to edit"
      onDoubleClick={() => setEditing(true)}
      onKeyDown={(e) => e.key === "Enter" && setEditing(true)}
    >
      {source.trim() ? (
        <Markdown text={source} />
      ) : (
        <span className="text-[12.5px] text-muted">
          Empty text cell — double-click to edit
        </span>
      )}
    </div>
  );
}

// ------------------------------------------------------------------ AI per cell

function CellAi({
  cell,
  editorKey,
  connectionId,
  hasError,
  onClose,
}: {
  cell: NotebookCell;
  editorKey: string;
  connectionId: string | null;
  hasError: boolean;
  onClose: () => void;
}) {
  const send = useAi((s) => s.send);
  const [text, setText] = useState("");
  const isSql = cell.kind === "sql";
  const go = (message: string, mode: Parameters<typeof send>[0]["mode"]) => {
    if (!connectionId)
      return useStore.getState().toast("Choose a connection first", "error");
    void send({
      message,
      mode,
      targetKey: isSql ? editorKey : null,
      connectionId,
    });
    onClose();
  };
  const quick = useMemo(
    () =>
      isSql
        ? [
            {
              label: "Explain",
              run: () => go("Explain what this cell's query does.", "explain"),
            },
            ...(hasError
              ? [
                  {
                    label: "Fix error",
                    run: () =>
                      go("Fix the error in this cell's query.", "fix_error"),
                  },
                ]
              : []),
            {
              label: "Optimize",
              run: () =>
                go(
                  "Suggest a faster version of this query and put it in the cell.",
                  "edit",
                ),
            },
            {
              label: "Analyze output",
              run: () =>
                go(
                  "Analyze this cell's result and summarize the findings.",
                  "analyze_result",
                ),
            },
          ]
        : [
            {
              label: "Write SQL for this",
              run: () => go(`Write SQL for: ${cell.source}`, "generate"),
            },
          ],
    [isSql, hasError, cell.source], // eslint-disable-line react-hooks/exhaustive-deps
  );
  return (
    <div className="mt-2 rounded-lg border border-accent/40 bg-accent/5 p-2">
      <form
        className="flex items-center gap-1.5"
        onSubmit={(e) => {
          e.preventDefault();
          if (text.trim())
            go(
              text.trim(),
              isSql ? (cell.source.trim() ? "edit" : "generate") : "chat",
            );
        }}
      >
        <Wand2 size={13} className="shrink-0 text-accent" />
        <input
          autoFocus
          className="field h-7 py-0"
          placeholder={
            isSql
              ? cell.source.trim()
                ? "Change this query… e.g. group by month"
                : "Describe the query to write…"
              : "Ask about this note…"
          }
          aria-label="Ask AI about this cell"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => e.key === "Escape" && onClose()}
        />
        <button
          type="submit"
          className="btn-primary h-7 py-0"
          disabled={!text.trim()}
        >
          Ask
        </button>
      </form>
      <div className="mt-1.5 flex flex-wrap gap-1">
        {quick.map((q) => (
          <button
            key={q.label}
            className="rounded-md border border-line bg-panel px-2 py-0.5 text-[11.5px] hover:bg-hover"
            onClick={q.run}
          >
            {q.label}
          </button>
        ))}
      </div>
    </div>
  );
}
