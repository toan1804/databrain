import { useEffect, useState } from "react";
import { MessageSquareText, Table2 } from "lucide-react";
import { shownStatement, useStore } from "../store";
import { kbd } from "../lib/platform";
import { DoneView, ErrorView, Messages, RunningView, RunSummary } from "./results/RunStatus";
import { ResultTabs } from "./results/ResultTabs";
import { CostBar } from "./results/CostBar";
import { TipsBar } from "./results/TipsBar";
import { ResultView } from "./results/ResultView";

export { ResultView };

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
  const keepResult = useStore((s) => s.keepResult);
  const [showMessages, setShowMessages] = useState(false);

  useEffect(() => setShowMessages(false), [run?.jobId]);

  if (!run) {
    if (compact) return null;
    return (
      <div className="flex h-full min-h-0 flex-col">
        <CostBar tabId={tabId} />
        <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-2 text-[12.5px] text-muted">
          <Table2 size={26} className="opacity-40" />
          <div>Run a query to see results</div>
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

  const withOutput = [...(run.kept ?? []), ...run.statements.filter((s) => s.result || s.error || s.status === "running")];
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
          onRelease={(s) => s.output && void keepResult(tabId, s.output, false)}
        />
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
          <ErrorView stmt={active} tabId={tabId} connectionId={connectionId} count={run.statements.length} />
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
