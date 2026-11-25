import { useEffect, useMemo, useState, type MouseEvent } from "react";
import { MessageSquareText, Plus, Table2 } from "lucide-react";
import { shownStatement, useStore } from "../store";
import { kbd } from "../lib/platform";
import { DoneView, ErrorView, Messages, RunningView, RunSummary } from "./results/RunStatus";
import { ResultTabs } from "./results/ResultTabs";
import { CostBar } from "./results/CostBar";
import { TipsBar } from "./results/TipsBar";
import { ResultView } from "./results/ResultView";
import { AddResultPicker } from "./results/AddResultPicker";

export { ResultView };

export interface ResultsPanelProps {
  tabId: string;
  /** Overrides for notebook cells (no tab of their own). */
  connectionId?: string | null;
  title?: string;
  /** No SQL editor behind these results (the table explorer): no "Fix with AI" or "Optimize". */
  noEditor?: boolean;
  compact?: boolean;
}

export function ResultsPanel({ tabId, connectionId, title, compact, noEditor = false }: ResultsPanelProps) {
  const run = useStore((s) => s.runs[tabId]);
  const setActive = useStore((s) => s.setActiveStatement);
  const keepResult = useStore((s) => s.keepResult);
  const removeAddedResult = useStore((s) => s.removeAddedResult);
  const [showMessages, setShowMessages] = useState(false);
  const [picker, setPicker] = useState<{ x: number; y: number } | null>(null);
  // Outputs already in this tab (its own and added ones): not offered again.
  const shown = useMemo(() => new Set([...(run?.statements ?? []), ...(run?.kept ?? []), ...(run?.parts ?? [])].flatMap((s) => (s.output ? [s.output.handle] : []))), [run]);
  const openPicker = (e: MouseEvent<HTMLElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    setPicker({ x: r.left, y: r.bottom + 4 });
  };
  // Not in notebook cells: they have no result tabs of their own to add to.
  const canAdd = !compact;
  const pickerEl = picker && <AddResultPicker tabId={tabId} x={picker.x} y={picker.y} shown={shown} onClose={() => setPicker(null)} />;

  useEffect(() => setShowMessages(false), [run?.jobId]);

  if (!run) {
    if (compact) return null;
    return (
      <div className="flex h-full min-h-0 flex-col">
        <CostBar tabId={tabId} />
        <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-2 text-[12.5px] text-muted">
          <Table2 size={26} className="opacity-40" />
          <div>Run a query to see results</div>
          {canAdd && (
            <button className="btn-ghost h-6 border border-line py-0 text-[11.5px]" onClick={openPicker}>
              <Plus size={12} /> Add a result from another tab
            </button>
          )}
          {pickerEl}
          <div className="flex gap-3 text-[11.5px]">
            <span>
              <span className="kbd">{kbd("⌘")}</span> <span className="kbd">↵</span> run statement
            </span>
            <span>
              <span className="kbd">{kbd("⇧")}</span> <span className="kbd">{kbd("⌘")}</span> <span className="kbd">↵</span> run all
            </span>
            <span>
              <span className="kbd">{kbd("⌘")}</span> <span className="kbd">{kbd("⌥")}</span> <span className="kbd">↑</span>/<span className="kbd">↓</span> previous / next statement
            </span>
          </div>
        </div>
      </div>
    );
  }

  // A combined query's parts follow the run's own results.
  const withOutput = [...(run.kept ?? []), ...run.statements.filter((s) => s.result || s.error || s.status === "running"), ...(run.parts ?? [])];
  const active = shownStatement(run);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-9 shrink-0 items-center gap-0.5 border-b border-line px-2">
        <ResultTabs
          statements={withOutput}
          active={showMessages ? null : run.activeIndex}
          onSelect={(index) => {
            setShowMessages(false);
            setActive(tabId, index);
          }}
          onRelease={(s) => s.output && void (s.added ? removeAddedResult(tabId, s.output.handle) : keepResult(tabId, s.output, false))}
        />
        {canAdd && (
          <button className="icon-btn h-6 w-6 shrink-0" aria-label="Add a result" title="Add a result from another tab (read-only here)" onClick={openPicker}>
            <Plus size={13} />
          </button>
        )}
        {pickerEl}
        <button
          onClick={() => setShowMessages(true)}
          className={`flex h-7 shrink-0 items-center gap-1.5 rounded-md px-2 text-[12px] ${
            showMessages ? "bg-hover text-fg" : "text-muted hover:text-fg"
          }`}
        >
          <MessageSquareText size={13} /> Messages
        </button>
        <div className="ml-1 flex shrink-0 items-center gap-2 pr-1 text-[11.5px] text-muted">
          <RunSummary run={run} />
        </div>
      </div>
      {!showMessages && <CostBar tabId={tabId} />}
      {/* Query tips and "Optimize" act on an editor's SQL: not in a tab without one (the table explorer). */}
      {!showMessages && !noEditor && <TipsBar run={run} tabId={tabId} />}
      <div className="min-h-0 flex-1">
        {showMessages ? (
          <Messages run={run} />
        ) : active?.result ? (
          <ResultView
            key={active.result.id}
            stmt={active}
            info={active.result}
            tabId={tabId}
            // An added result (or a part) is another connection's maybe: its own.
            connectionId={(active.added || active.part) && active.output ? active.output.connection_id : connectionId}
            title={active.part ? active.part.name : active.added && active.output ? (active.output.name ?? active.output.handle) : title}
            readOnly={!!active.added || !!active.part}
          />
        ) : active?.error ? (
          <ErrorView stmt={active} tabId={tabId} connectionId={connectionId} count={run.statements.length} noEditor={noEditor} />
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
