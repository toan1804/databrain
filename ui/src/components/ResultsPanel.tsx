import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import {
  AlertCircle,
  ArrowDown,
  ArrowUp,
  Ban,
  BarChart3,
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  Copy,
  Download,
  Filter,
  ListFilter,
  Loader2,
  MessageSquareText,
  Search,
  Sparkles,
  Table2,
  Wrench,
  X,
  Lightbulb,
  ChevronRight,
} from "lucide-react";
import { EditorView } from "@codemirror/view";
import { useAi } from "../aiStore";
import { editorBridge } from "../editorBridge";
import { ChartView } from "./ChartView";
import { OutputChip } from "./OutputChip";
import { mentionInAi, outputLabel } from "../outputs";
import { planFind } from "../lib/find";
import { api, toError } from "../lib/api";
import type { ColumnStats, ExportFormat, FilterOp, ResultInfo, ViewSpec } from "../lib/types";
import {
  FILTER_OPS,
  emptyView,
  filterLabel,
  formatBytes,
  formatCount,
  formatDuration,
  sqlPreview,
  uid,
  upsertFilter,
} from "../lib/util";
import { useStore, type StatementRun, type TabRun } from "../store";
import { optimizePrompt } from "../queryTips";
import { ResultGrid, type GridHandle } from "./ResultGrid";
import { Modal, Popover } from "./ui";

export interface ResultsPanelProps {
  tabId: string;
  /** Overrides for notebook cells (no tab of their own). */
  connectionId?: string | null;
  title?: string;
  compact?: boolean;
}

export function ResultsPanel({ tabId, connectionId, title, compact }: ResultsPanelProps) {
  const run = useStore((s) => s.runs[tabId]);
  const setActive = useStore((s) => s.setActiveStatement);
  const [showMessages, setShowMessages] = useState(false);

  useEffect(() => setShowMessages(false), [run?.jobId]);

  if (!run) {
    if (compact) return null;
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 text-[12.5px] text-muted">
        <Table2 size={26} className="opacity-40" />
        <div>Run a query to see results</div>
        <div className="flex gap-3 text-[11.5px]">
          <span>
            <span className="kbd">⌘</span> <span className="kbd">↵</span> run statement
          </span>
          <span>
            <span className="kbd">⇧</span> <span className="kbd">⌘</span> <span className="kbd">↵</span> run all
          </span>
        </div>
      </div>
    );
  }

  const withOutput = run.statements.filter((s) => s.result || s.error || s.status === "running");
  const active = run.activeIndex !== null ? run.statements[run.activeIndex] : undefined;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-9 shrink-0 items-center gap-0.5 border-b border-line px-2">
        {withOutput.map((s) => {
          const isActive = !showMessages && run.activeIndex === s.plan.index;
          return (
            <button
              key={s.plan.index}
              onClick={() => {
                setShowMessages(false);
                setActive(tabId, s.plan.index);
              }}
              title={s.plan.sql}
              className={`flex h-7 max-w-[220px] items-center gap-1.5 rounded-md px-2 text-[12px] ${
                isActive ? "bg-hover text-fg" : "text-muted hover:text-fg"
              }`}
            >
              <StatusIcon s={s} />
              <span className="truncate">
                {withOutput.length > 1 ? `${s.plan.index + 1}. ` : ""}
                {s.result ? "Result" : s.error ? (s.status === "cancelled" ? "Cancelled" : "Error") : "Running"}
              </span>
              {s.result && <span className="text-[11px] text-muted">{formatCount(s.result.total_rows)}</span>}
              {s.output && <span className="font-mono text-[10.5px] text-muted">{outputLabel(s.output)}</span>}
            </button>
          );
        })}
        <button
          onClick={() => setShowMessages(true)}
          className={`flex h-7 items-center gap-1.5 rounded-md px-2 text-[12px] ${
            showMessages ? "bg-hover text-fg" : "text-muted hover:text-fg"
          }`}
        >
          <MessageSquareText size={13} /> Messages
        </button>
        <div className="ml-auto flex items-center gap-2 pr-1 text-[11.5px] text-muted">
          <RunSummary run={run} />
        </div>
      </div>
      {!showMessages && <TipsBar run={run} tabId={tabId} />}
      <div className="min-h-0 flex-1">
        {showMessages ? (
          <Messages run={run} />
        ) : active?.result ? (
          <ResultView
            key={active.result.id}
            stmt={active}
            info={active.result}
            tabId={tabId}
            connectionId={connectionId}
            title={title}
          />
        ) : active?.error ? (
          <ErrorView stmt={active} tabId={tabId} connectionId={connectionId} />
        ) : active?.status === "done" ? (
          <DoneView stmt={active} />
        ) : run.running ? (
          <RunningView run={run} tabId={tabId} />
        ) : (
          <Messages run={run} />
        )}
      </div>
    </div>
  );
}

/** Suggestions for the statement shown, from its tables' indexes / partitions / cluster keys. */
/**
 * Select the statement in the editor (the AI's rewrite replaces exactly it)
 * and send it with its tips and table layouts, nothing else.
 */
function askToOptimize(run: TabRun, tabId: string, index: number) {
  const st = run.tips?.statements.find((s) => s.index === index);
  const message = run.tips && optimizePrompt(run.tips, index, run.statements[index]?.durationMs);
  if (!st || !message) return;
  const v = editorBridge.get(tabId);
  const end = st.start + st.sql.length;
  if (!v || end > v.state.doc.length || v.state.doc.sliceString(st.start, end) !== st.sql) {
    useStore.getState().toast("The statement was edited since it ran; run it again to get tips for the new version", "info");
    return;
  }
  v.dispatch({ selection: { anchor: st.start, head: end }, effects: EditorView.scrollIntoView(st.start, { y: "center" }) });
  void useAi.getState().send({
    message,
    mode: "optimize",
    targetKey: tabId,
    connectionId: run.connectionId,
    // Only the statement: not the rest of the editor, the last error or the result.
    context: { editor_sql: null, selection: st.sql, last_error: null, result_id: null },
  });
}

function TipsBar({ run, tabId }: { run: TabRun; tabId: string }) {
  const [hidden, setHidden] = useState<string | null>(null);
  const [open, setOpen] = useState(true);
  // The active statement's tips, or (while a script runs) the running one's.
  const running = run.statements.find((s) => s.status === "running");
  const idx = run.activeIndex ?? running?.plan.index ?? null;
  const tips = (run.tips?.tips ?? []).filter((t) => idx === null || t.statementIndex === idx);
  if (!tips.length || hidden === run.jobId) return null;
  const stmt = idx !== null ? run.statements[idx] : undefined;
  const warn = tips.some((t) => t.level === "warn");
  const show = (from: number, to: number) => {
    const v = editorBridge.get(tabId);
    if (!v || to > v.state.doc.length) return;
    v.dispatch({ selection: { anchor: from, head: to }, effects: EditorView.scrollIntoView(from, { y: "center" }) });
    v.focus();
  };
  return (
    <div className={`shrink-0 border-b border-line text-[12px] ${warn ? "bg-warning/5" : "bg-accent/5"}`} role="region" aria-label="Query tips">
      <div className="flex items-center gap-1.5 px-2 py-1">
        <button className="flex items-center gap-1.5 font-medium" onClick={() => setOpen(!open)} aria-expanded={open}>
          <Lightbulb size={13} className={warn ? "text-warning" : "text-accent"} />
          {tips.length} tip{tips.length === 1 ? "" : "s"} to speed up this {stmt?.status === "running" ? "slow query" : "query"}
          {stmt?.status === "running" && stmt.startedAt !== undefined && <span className="font-normal text-muted">· still running (over {formatDuration(Date.now() - stmt.startedAt)})</span>}
          {stmt?.status !== "running" && stmt?.durationMs !== undefined && <span className="font-normal text-muted">· ran in {formatDuration(stmt.durationMs)}</span>}
          {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        </button>
        {stmt && run.tips && (
          <button
            className="btn-ghost ml-auto py-0.5 text-[11.5px]"
            title="Ask the AI to rewrite this statement, using only the statement, its tips and its tables' layouts"
            onClick={() => askToOptimize(run, tabId, stmt.plan.index)}
          >
            <Sparkles size={12} /> Improve with AI
          </button>
        )}
        <button className={`icon-btn h-5 w-5 ${stmt && run.tips ? "" : "ml-auto"}`} aria-label="Hide tips" title="Hide tips for this run" onClick={() => setHidden(run.jobId)}>
          <X size={12} />
        </button>
      </div>
      {open && (
        <ul className="max-h-32 space-y-0.5 overflow-auto px-2 pb-1.5">
          {tips.map((t, i) => (
            <li key={i}>
              <button className="flex w-full items-start gap-1.5 rounded px-1 py-0.5 text-left hover:bg-hover" onClick={() => show(t.from, t.to)} title="Show in the editor">
                <span className={`mt-1 h-1.5 w-1.5 shrink-0 rounded-full ${t.level === "warn" ? "bg-warning" : "bg-accent"}`} />
                <span className="min-w-0">{t.message}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function StatusIcon({ s }: { s: StatementRun }) {
  if (s.status === "running" || s.status === "pending") return <Loader2 size={12} className="animate-spin text-accent" />;
  if (s.status === "error") return <AlertCircle size={12} className="text-danger" />;
  if (s.status === "cancelled") return <Ban size={12} className="text-muted" />;
  return <CheckCircle2 size={12} className="text-success" />;
}

function RunSummary({ run }: { run: TabRun }) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!run.running) return;
    const t = setInterval(() => setNow(Date.now()), 100);
    return () => clearInterval(t);
  }, [run.running]);
  if (run.running) {
    const done = run.statements.filter((s) => s.status === "done").length;
    return (
      <span className="flex items-center gap-1.5">
        <Loader2 size={12} className="animate-spin text-accent" />
        {run.statements.length > 1 && `${done}/${run.statements.length} · `}
        {formatDuration(now - run.startedAt)}
      </span>
    );
  }
  return (
    <span>
      {run.finishedStatus === "cancelled" ? "Cancelled · " : run.finishedStatus === "error" ? "Failed · " : ""}
      {run.durationMs !== undefined && formatDuration(run.durationMs)}
    </span>
  );
}

function RunningView({ run, tabId }: { run: TabRun; tabId: string }) {
  const cancel = useStore((s) => s.cancelTab);
  const cur = run.statements.find((s) => s.status === "running");
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 text-[12.5px] text-muted">
      <Loader2 size={22} className="animate-spin text-accent" />
      <div className="max-w-[70%] truncate font-mono text-[11.5px]">{cur ? sqlPreview(cur.plan.sql, 120) : "Connecting…"}</div>
      {cur?.downloading && cur.serverMs !== undefined && <div>Finished on the server in {formatDuration(cur.serverMs)} · downloading the result</div>}
      {cur?.progressRows ? <div>{formatCount(cur.progressRows)} rows fetched</div> : null}
      {cur?.notices.map((n, i) => (
        <div key={i} className="max-w-[70%] select-text rounded-md border border-warning/40 bg-warning/5 px-2 py-1 text-center text-[11.5px] text-warning">
          {n}
        </div>
      ))}
      <button className="btn-ghost border border-line" onClick={() => cancel(tabId)}>
        <X size={14} /> Cancel <span className="kbd ml-1">⌘.</span>
      </button>
    </div>
  );
}

function ErrorView({ stmt, tabId, connectionId }: { stmt: StatementRun; tabId: string; connectionId?: string | null }) {
  const e = stmt.error!;
  const cancelled = stmt.status === "cancelled";
  const send = useAi((s) => s.send);
  const fix = () =>
    void send({
      message: `Fix this error:\n${e.message}`,
      mode: "fix_error",
      targetKey: tabId,
      connectionId: connectionId ?? undefined,
      context: { last_error: e.message },
    });
  return (
    <div className="h-full overflow-auto p-4">
      <div
        className={`rounded-lg border p-3 ${cancelled ? "border-line bg-panel-2" : "border-danger/40 bg-danger/10"}`}
        role="alert"
      >
        <div className="mb-1 flex items-center gap-2 text-[13px] font-semibold">
          {cancelled ? <Ban size={15} className="text-muted" /> : <AlertCircle size={15} className="text-danger" />}
          {cancelled ? "Query cancelled" : `Statement ${stmt.plan.index + 1} failed`}
          {e.code && <span className="rounded bg-panel px-1.5 font-mono text-[11px] text-muted">{e.code}</span>}
        </div>
        {!cancelled && <pre className="whitespace-pre-wrap break-words font-mono text-[12px] select-text">{e.message}</pre>}
        <div className="mt-2 font-mono text-[11px] text-muted">{sqlPreview(stmt.plan.sql, 200)}</div>
        {!cancelled && (
          <button className="btn-ghost mt-2 border border-line py-1" onClick={fix}>
            <Wrench size={12} /> Fix with AI
          </button>
        )}
      </div>
    </div>
  );
}

function DoneView({ stmt }: { stmt: StatementRun }) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 text-[13px]">
      <CheckCircle2 size={22} className="text-success" />
      <div>
        {stmt.rowsAffected !== null && stmt.rowsAffected !== undefined
          ? `${formatCount(stmt.rowsAffected)} row${stmt.rowsAffected === 1 ? "" : "s"} affected`
          : "Statement executed"}
        {stmt.durationMs !== undefined && <span className="text-muted"> · {formatDuration(stmt.durationMs)}</span>}
      </div>
      <div className="max-w-[70%] truncate font-mono text-[11.5px] text-muted">{sqlPreview(stmt.plan.sql, 120)}</div>
    </div>
  );
}

function Messages({ run }: { run: TabRun }) {
  return (
    <div className="h-full overflow-auto p-3 font-mono text-[12px] select-text">
      {run.statements.map((s) => (
        <div key={s.plan.index} className="mb-2 border-b border-line pb-2 last:border-0">
          <div className="flex items-center gap-2">
            <StatusIcon s={s} />
            <span className="text-muted">#{s.plan.index + 1}</span>
            <span className="truncate">{sqlPreview(s.plan.sql, 140)}</span>
          </div>
          <div className="mt-1 pl-6 text-[11.5px] text-muted">
            {s.status === "done" &&
              (s.result
                ? `${formatCount(s.result.total_rows)} rows${s.result.truncated ? " (limited)" : ""}`
                : s.rowsAffected !== null && s.rowsAffected !== undefined
                  ? `${formatCount(s.rowsAffected)} rows affected`
                  : "OK")}
            {s.durationMs !== undefined && ` · ${formatDuration(s.durationMs)}`}
            {s.serverMs !== undefined && s.durationMs !== undefined && s.durationMs - s.serverMs > 1000 && ` (${formatDuration(s.serverMs)} on the server, the rest downloading)`}
            {s.status === "pending" && "Not executed"}
          </div>
          {s.error && s.status === "error" && <div className="mt-1 whitespace-pre-wrap pl-6 text-danger">{s.error.message}</div>}
          {s.notices.map((n, i) => (
            <div key={i} className="mt-1 pl-6 text-warning">
              {n}
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

// ------------------------------------------------------------------ result view

export function ResultView({
  stmt,
  info,
  tabId,
  connectionId,
  title,
}: {
  stmt: Pick<StatementRun, "durationMs" | "output">;
  info: ResultInfo;
  tabId: string;
  connectionId?: string | null;
  title?: string;
}) {
  const [mode, setMode] = useState<"grid" | "chart">("grid");
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const connId = connectionId !== undefined ? connectionId : tab?.connection_id;
  const conn = useStore((s) => s.connections.find((c) => c.id === connId));
  const send = useAi((s) => s.send);
  const [view, setView] = useState<ViewSpec>(emptyView);
  const [quick, setQuick] = useState("");
  const [viewRows, setViewRows] = useState(info.total_rows);
  const [findOpen, setFindOpen] = useState(false);
  const [findQuery, setFindQuery] = useState("");
  const [find, setFind] = useState<{ matches: { row: number; col: number }[]; truncated: boolean }>({ matches: [], truncated: false });
  const [findIdx, setFindIdx] = useState(0);
  /** Column the find bar is limited to (null = all columns). */
  const [findScope, setFindScope] = useState<number | null>(null);
  const columnNames = useMemo(() => info.columns.map((c) => c.name), [info.columns]);
  const plan = useMemo(() => planFind(findQuery, columnNames, findScope), [findQuery, columnNames, findScope]);
  const [headerMenu, setHeaderMenu] = useState<{ col: number; x: number; y: number } | null>(null);
  const [exportOpen, setExportOpen] = useState(false);
  const grid = useRef<GridHandle>(null);
  const findInput = useRef<HTMLInputElement>(null);
  const toast = useStore((s) => s.toast);

  // Debounced quick filter.
  useEffect(() => {
    const t = setTimeout(() => setView((v) => ({ ...v, quick_filter: quick.trim() || null })), 250);
    return () => clearTimeout(t);
  }, [quick]);

  // Find within the current view.
  useEffect(() => {
    if (!findOpen || !plan.term) {
      setFind({ matches: [], truncated: false });
      return;
    }
    const t = setTimeout(async () => {
      try {
        const r = await api.findInResult(info.id, view, plan.term, 10_000, plan.columns);
        setFind(r);
        setFindIdx(0);
        if (r.matches[0]) grid.current?.scrollToCell(r.matches[0].col, r.matches[0].row);
      } catch (e) {
        toast(toError(e).message, "error");
      }
    }, 200);
    return () => clearTimeout(t);
  }, [findOpen, plan.term, plan.columns?.join(","), view, info.id, toast]); // eslint-disable-line react-hooks/exhaustive-deps

  /** Scroll to a column (keeps the current match row when there is one). */
  const jumpToColumn = (col: number) => grid.current?.scrollToCell(col, find.matches[findIdx]?.row ?? 0);

  const step = (d: number) => {
    const n = find.matches.length;
    if (n === 0) return;
    const i = (findIdx + d + n) % n;
    setFindIdx(i);
    grid.current?.scrollToCell(find.matches[i].col, find.matches[i].row);
  };

  const openFind = useCallback(() => {
    setFindOpen(true);
    setTimeout(() => findInput.current?.select(), 0);
  }, []);

  const filtered = view.filters.length > 0 || !!view.quick_filter;

  return (
    <div
      className="flex h-full min-h-0 flex-col"
      onKeyDownCapture={(e) => {
        if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f") {
          e.preventDefault();
          e.stopPropagation();
          openFind();
        }
      }}
    >
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-line px-2">
        {stmt.output && <OutputChip output={stmt.output} />}
        <div className="flex rounded-md border border-line p-0.5" role="radiogroup" aria-label="View">
          {(["grid", "chart"] as const).map((m) => (
            <button
              key={m}
              role="radio"
              aria-checked={mode === m}
              title={m === "grid" ? "Table" : "Chart"}
              onClick={() => setMode(m)}
              className={`flex h-5 w-6 items-center justify-center rounded ${mode === m ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
            >
              {m === "grid" ? <Table2 size={12} /> : <BarChart3 size={12} />}
            </button>
          ))}
        </div>
        <div className="relative w-56">
          <ListFilter size={13} className="absolute left-2 top-1/2 -translate-y-1/2 text-muted" />
          <input
            className="field py-1 pl-7 pr-6"
            placeholder="Filter rows…"
            aria-label="Filter rows"
            value={quick}
            onChange={(e) => setQuick(e.target.value)}
          />
          {quick && (
            <button className="absolute right-1.5 top-1/2 -translate-y-1/2 text-muted hover:text-fg" aria-label="Clear filter" onClick={() => setQuick("")}>
              <X size={12} />
            </button>
          )}
        </div>
        <div className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto">
          {view.filters.map((f, i) => (
            <span key={i} className="flex shrink-0 items-center gap-1 rounded-md bg-accent/12 px-2 py-0.5 text-[11.5px] text-accent">
              <Filter size={10} />
              {filterLabel(f, info.columns[f.column]?.name ?? `#${f.column}`)}
              <button
                aria-label="Remove filter"
                className="opacity-70 hover:opacity-100"
                onClick={() => setView((v) => ({ ...v, filters: v.filters.filter((_, j) => j !== i) }))}
              >
                <X size={11} />
              </button>
            </span>
          ))}
          {view.sort.length > 0 && (
            <button className="shrink-0 text-[11.5px] text-muted hover:text-fg" onClick={() => setView((v) => ({ ...v, sort: [] }))}>
              Clear sort
            </button>
          )}
        </div>
        <button className="icon-btn" title="Find (⌘F)" aria-label="Find in results" onClick={openFind}>
          <Search size={14} />
        </button>
        <button className="icon-btn" title="Copy selection (⌘C)" aria-label="Copy selection" onClick={() => grid.current?.copySelection("tsv", true)}>
          <Copy size={14} />
        </button>
        <button
          className="btn-ghost border border-line py-1"
          title={stmt.output ? `Open the AI assistant with @${outputLabel(stmt.output)} mentioned, then ask your own question` : "Ask the AI to analyze this result"}
          onClick={() => {
            if (stmt.output) {
              mentionInAi(stmt.output);
              return;
            }
            void send({
              message: "Analyze this result and summarize the key findings.",
              mode: "analyze_result",
              targetKey: tabId,
              connectionId: connId ?? undefined,
              context: { result_id: info.id, mentions: [] },
            });
          }}
        >
          <Sparkles size={13} /> Ask AI
        </button>
        <button className="btn-ghost border border-line py-1" onClick={() => setExportOpen(true)}>
          <Download size={13} /> Export
        </button>
      </div>

      {findOpen && (
        <div className="flex h-9 shrink-0 items-center gap-2 border-b border-line bg-panel-2 px-2">
          <Search size={13} className="text-muted" />
          <input
            ref={findInput}
            className="field w-64 py-0.5"
            placeholder={findScope === null ? "Find values or columns (column: value)" : `Find in ${columnNames[findScope]}`}
            aria-label="Find in results"
            title="Type text to find in cells. “country: viet” searches only the country column; matching column names are listed to jump to."
            value={findQuery}
            onChange={(e) => setFindQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") step(e.shiftKey ? -1 : 1);
              if (e.key === "Escape") setFindOpen(false);
            }}
          />
          <select
            className="field h-6 w-40 py-0 text-[11.5px]"
            aria-label="Search in column"
            title="Search in one column"
            value={findScope ?? ""}
            onChange={(e) => {
              const v = e.target.value === "" ? null : Number(e.target.value);
              setFindScope(v);
              if (v !== null) jumpToColumn(v);
              findInput.current?.focus();
            }}
          >
            <option value="">All columns</option>
            {columnNames.map((c, i) => (
              <option key={i} value={i}>
                {c}
              </option>
            ))}
          </select>
          <span className="min-w-[80px] text-[11.5px] text-muted" aria-live="polite">
            {plan.term
              ? find.matches.length
                ? `${findIdx + 1} / ${formatCount(find.matches.length)}${find.truncated ? "+" : ""}`
                : "No matches"
              : ""}
          </span>
          <button className="icon-btn h-6 w-6" aria-label="Previous match" onClick={() => step(-1)}>
            <ChevronUp size={14} />
          </button>
          <button className="icon-btn h-6 w-6" aria-label="Next match" onClick={() => step(1)}>
            <ChevronDown size={14} />
          </button>
          {plan.nameMatches.length > 0 && (
            <div className="flex min-w-0 items-center gap-1 overflow-hidden text-[11.5px]">
              <span className="shrink-0 text-muted">Columns:</span>
              {plan.nameMatches.slice(0, 6).map((i) => (
                <button
                  key={i}
                  className="shrink-0 rounded border border-line px-1.5 font-mono text-[11px] hover:border-accent hover:text-accent"
                  title={`Go to column ${columnNames[i]} and search only in it`}
                  onClick={() => {
                    jumpToColumn(i);
                    setFindScope(i);
                    setFindQuery("");
                    findInput.current?.focus();
                  }}
                >
                  {columnNames[i]}
                </button>
              ))}
              {plan.nameMatches.length > 6 && <span className="text-muted">+{plan.nameMatches.length - 6}</span>}
            </div>
          )}
          <button className="icon-btn ml-auto h-6 w-6" aria-label="Close find" onClick={() => setFindOpen(false)}>
            <X size={14} />
          </button>
        </div>
      )}

      <div className="min-h-0 flex-1">
        {mode === "chart" ? (
          <ChartView info={info} view={view} />
        ) : (
        <ResultGrid
          ref={grid}
          info={info}
          view={view}
          onViewChange={setView}
          onViewRows={setViewRows}
          onHeaderMenu={(col, x, y) => setHeaderMenu({ col, x, y })}
          findMatches={find.matches}
          findCurrent={findIdx}
          dialect={conn?.config.kind}
        />
        )}
      </div>

      <div className="flex h-7 shrink-0 items-center gap-3 border-t border-line px-3 text-[11.5px] text-muted">
        <span>
          {filtered ? `${formatCount(viewRows)} of ${formatCount(info.total_rows)} rows` : `${formatCount(info.total_rows)} rows`}
        </span>
        {info.truncated && (
          <span className="rounded bg-warning/15 px-1.5 text-warning" title="Increase the row limit in the editor toolbar to fetch more">
            limited to {formatCount(info.total_rows)}
          </span>
        )}
        <span>{info.columns.length} columns</span>
        {stmt.durationMs !== undefined && <span>{formatDuration(stmt.durationMs)}</span>}
        <span className="ml-auto">{formatBytes(info.bytes)}</span>
      </div>

      {headerMenu && (
        <ColumnMenu
          info={info}
          col={headerMenu.col}
          x={headerMenu.x}
          y={headerMenu.y}
          view={view}
          onView={setView}
          onClose={() => setHeaderMenu(null)}
        />
      )}
      {exportOpen && (
        <ExportDialog
          info={info}
          view={view}
          viewRows={viewRows}
          dialect={conn?.config.kind}
          defaultName={title ?? tab?.title ?? "result"}
          onClose={() => setExportOpen(false)}
        />
      )}
    </div>
  );
}

// ------------------------------------------------------------------ column menu

function ColumnMenu({
  info,
  col,
  x,
  y,
  view,
  onView,
  onClose,
}: {
  info: ResultInfo;
  col: number;
  x: number;
  y: number;
  view: ViewSpec;
  onView: (v: ViewSpec) => void;
  onClose: () => void;
}) {
  const meta = info.columns[col];
  const existing = view.filters.find((f) => f.column === col);
  const [op, setOp] = useState<FilterOp>(existing?.op ?? (meta.family === "number" ? "equals" : "contains"));
  const [value, setValue] = useState(existing?.value ?? "");
  const [stats, setStats] = useState<ColumnStats | null>(null);
  const [statsError, setStatsError] = useState<string | null>(null);
  const needsValue = FILTER_OPS.find((o) => o.op === op)?.needsValue ?? true;

  useEffect(() => {
    let alive = true;
    api
      .columnStats(info.id, view, col)
      .then((s) => alive && setStats(s))
      .catch((e) => alive && setStatsError(toError(e).message));
    return () => {
      alive = false;
    };
  }, [info.id, view, col]);

  const apply = () => {
    onView({ ...view, filters: upsertFilter(view.filters.filter((f) => f.column !== col), { column: col, op, value }) });
    onClose();
  };

  const maxTop = stats?.top[0]?.count ?? 1;

  return (
    <Popover x={x} y={y} onClose={onClose} className="w-72 p-0">
      <div className="border-b border-line px-3 py-2">
        <div className="truncate font-semibold">{meta.name}</div>
        <div className="font-mono text-[11px] text-muted">{meta.db_type ?? meta.data_type}</div>
      </div>
      <div className="flex gap-1 border-b border-line p-1.5">
        <button className="btn-ghost flex-1 justify-center py-1" onClick={() => { onView({ ...view, sort: [{ column: col, descending: false }] }); onClose(); }}>
          <ArrowUp size={13} /> Asc
        </button>
        <button className="btn-ghost flex-1 justify-center py-1" onClick={() => { onView({ ...view, sort: [{ column: col, descending: true }] }); onClose(); }}>
          <ArrowDown size={13} /> Desc
        </button>
      </div>
      <form
        className="space-y-1.5 border-b border-line p-2"
        onSubmit={(e) => {
          e.preventDefault();
          apply();
        }}
      >
        <div className="text-[11px] font-medium text-muted">Filter</div>
        <select className="field py-1" value={op} aria-label="Filter operator" onChange={(e) => setOp(e.target.value as FilterOp)}>
          {FILTER_OPS.map((o) => (
            <option key={o.op} value={o.op}>
              {o.label}
            </option>
          ))}
        </select>
        {needsValue && (
          <input className="field py-1" autoFocus value={value} aria-label="Filter value" placeholder="Value" onChange={(e) => setValue(e.target.value)} />
        )}
        <div className="flex justify-end gap-1 pt-0.5">
          {existing && (
            <button
              type="button"
              className="btn-ghost py-1"
              onClick={() => {
                onView({ ...view, filters: view.filters.filter((f) => f.column !== col) });
                onClose();
              }}
            >
              Remove
            </button>
          )}
          <button type="submit" className="btn-primary py-1">
            Apply
          </button>
        </div>
      </form>
      <div className="p-2">
        <div className="mb-1 flex items-center gap-1 text-[11px] font-medium text-muted">
          <BarChart3 size={11} /> Statistics
        </div>
        {statsError && <div className="text-[11.5px] text-danger">{statsError}</div>}
        {!stats && !statsError && <Loader2 size={13} className="animate-spin text-muted" />}
        {stats && (
          <div className="space-y-1.5 text-[11.5px]">
            <div className="grid grid-cols-3 gap-1">
              <Stat label="Rows" value={formatCount(stats.count)} />
              <Stat label="Distinct" value={formatCount(stats.distinct)} />
              <Stat label="Nulls" value={formatCount(stats.nulls)} />
            </div>
            {stats.min !== null && (
              <div className="grid grid-cols-2 gap-1">
                <Stat label="Min" value={stats.min} />
                <Stat label="Max" value={stats.max ?? ""} />
              </div>
            )}
            <div className="max-h-40 space-y-0.5 overflow-auto">
              {stats.top.map((t, i) => (
                <button
                  key={i}
                  className="relative flex w-full items-center justify-between overflow-hidden rounded px-1.5 py-0.5 text-left hover:bg-hover"
                  title="Filter by this value"
                  onClick={() => {
                    onView({
                      ...view,
                      filters: upsertFilter(view.filters.filter((f) => f.column !== col), {
                        column: col,
                        op: t.value === null ? "is_null" : "equals",
                        value: t.value ?? "",
                      }),
                    });
                    onClose();
                  }}
                >
                  <span className="absolute inset-y-0 left-0 bg-accent/12" style={{ width: `${(t.count / maxTop) * 100}%` }} />
                  <span className={`relative truncate font-mono ${t.value === null ? "italic text-muted" : ""}`}>{t.value ?? "NULL"}</span>
                  <span className="relative ml-2 text-muted">{formatCount(t.count)}</span>
                </button>
              ))}
            </div>
          </div>
        )}
      </div>
    </Popover>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0 rounded bg-panel-2 px-1.5 py-1">
      <div className="text-[10px] text-muted">{label}</div>
      <div className="truncate font-mono" title={value}>
        {value}
      </div>
    </div>
  );
}

// ------------------------------------------------------------------ export

const FORMATS: { value: ExportFormat; label: string; ext: string }[] = [
  { value: "csv", label: "CSV", ext: "csv" },
  { value: "tsv", label: "TSV", ext: "tsv" },
  { value: "json", label: "JSON (array)", ext: "json" },
  { value: "ndjson", label: "JSON Lines", ext: "ndjson" },
  { value: "markdown", label: "Markdown table", ext: "md" },
  { value: "sql_insert", label: "SQL INSERT statements", ext: "sql" },
  { value: "parquet", label: "Parquet (zstd)", ext: "parquet" },
  { value: "xlsx", label: "Excel workbook", ext: "xlsx" },
];

function ExportDialog({
  info,
  view,
  viewRows,
  dialect,
  defaultName,
  onClose,
}: {
  info: ResultInfo;
  view: ViewSpec;
  viewRows: number;
  dialect: string | undefined;
  defaultName: string;
  onClose: () => void;
}) {
  const toast = useStore((s) => s.toast);
  const [format, setFormat] = useState<ExportFormat>("csv");
  const [header, setHeader] = useState(true);
  const [scope, setScope] = useState<"view" | "fetched" | "all">("view");
  const useView = scope === "view";
  const exportId = useRef<string | null>(null);
  const [started, setStarted] = useState<number | null>(null);
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (started === null) return;
    const t = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(t);
  }, [started]);
  const [table, setTable] = useState("my_table");
  const [busy, setBusy] = useState(false);
  const fmt = FORMATS.find((f) => f.value === format)!;
  const filtered = view.filters.length > 0 || !!view.quick_filter || view.sort.length > 0;
  const fileBase = useMemo(() => defaultName.replace(/[^\w.-]+/g, "_") || "result", [defaultName]);

  const run = async () => {
    const path = await saveDialog({
      defaultPath: `${fileBase}.${fmt.ext}`,
      filters: [{ name: fmt.label, extensions: [fmt.ext] }],
    });
    if (typeof path !== "string") return;
    setBusy(true);
    const options = { format, header, table_name: table || "my_table", dialect: (dialect as never) ?? null };
    try {
      let n: number;
      if (scope === "all") {
        exportId.current = uid();
        setStarted(Date.now());
        n = await api.exportFullResult(info.id, options, path, exportId.current);
      } else n = await api.exportResult(info.id, useView ? view : emptyView(), options, path);
      toast(`Exported ${formatCount(n)} rows to ${path.split(/[\\/]/).pop()}`, "success");
      onClose();
    } catch (e) {
      const err = toError(e);
      toast(err.kind === "cancelled" ? "Export cancelled" : err.message, err.kind === "cancelled" ? "info" : "error");
    } finally {
      exportId.current = null;
      setStarted(null);
      setBusy(false);
    }
  };
  const cancelExport = () => exportId.current && void api.cancelExport(exportId.current);

  return (
    <Modal
      title="Export results"
      onClose={() => (busy && scope === "all" ? cancelExport() : onClose())}
      width={440}
      footer={
        <>
          {started !== null && <span className="mr-auto text-[12px] text-muted">Running the query and writing the file… {formatDuration(now - started)}</span>}
          <button className="btn-ghost" onClick={() => (busy && scope === "all" ? cancelExport() : onClose())}>
            {busy && scope === "all" ? "Stop" : "Cancel"}
          </button>
          <button className="btn-primary" onClick={run} disabled={busy}>
            {busy ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />} Export…
          </button>
        </>
      }
    >
      <div className="space-y-4">
        <div>
          <div className="mb-1.5 text-[11.5px] font-medium text-muted">Format</div>
          <div className="grid grid-cols-2 gap-1.5">
            {FORMATS.map((f) => (
              <button
                key={f.value}
                onClick={() => setFormat(f.value)}
                aria-pressed={format === f.value}
                className={`rounded-lg border px-3 py-2 text-left text-[12.5px] ${
                  format === f.value ? "border-accent bg-accent/10" : "border-line hover:bg-hover"
                }`}
              >
                {f.label}
              </button>
            ))}
          </div>
        </div>
        {(format === "csv" || format === "tsv") && (
          <label className="flex items-center gap-2 text-[13px]">
            <input type="checkbox" checked={header} onChange={(e) => setHeader(e.target.checked)} /> Include header row
          </label>
        )}
        {format === "sql_insert" && (
          <div>
            <div className="mb-1 text-[11.5px] font-medium text-muted">Table name</div>
            <input className="field font-mono" value={table} onChange={(e) => setTable(e.target.value)} />
          </div>
        )}
        <div className="space-y-1.5 text-[13px]">
          <label className="flex items-center gap-2">
            <input type="radio" checked={scope === "view"} disabled={busy} onChange={() => setScope("view")} />
            Current view {filtered ? "(filtered/sorted)" : ""} — {formatCount(viewRows)} rows
          </label>
          <label className="flex items-center gap-2">
            <input type="radio" checked={scope === "fetched"} disabled={busy} onChange={() => setScope("fetched")} />
            All fetched rows — {formatCount(info.total_rows)} rows
          </label>
          <label className="flex items-start gap-2">
            <input type="radio" className="mt-1" checked={scope === "all"} disabled={busy} onChange={() => setScope("all")} />
            <span>
              All rows of the query, ignoring the row limit
              <span className="block text-[11.5px] text-muted">Runs the query again and writes every row straight to the file (filters and sorting of the grid don't apply).</span>
            </span>
          </label>
          {info.truncated && scope !== "all" && (
            <p className="text-[11.5px] text-warning">
              This result was limited to {formatCount(info.total_rows)} rows. Choose "All rows of the query" to export everything.
            </p>
          )}
        </div>
      </div>
    </Modal>
  );
}
