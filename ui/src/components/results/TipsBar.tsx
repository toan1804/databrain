import { useState } from "react";
import { ChevronDown, ChevronRight, Lightbulb, Sparkles, X } from "lucide-react";
import { EditorView } from "@codemirror/view";
import { useAi } from "../../aiStore";
import { editorBridge } from "../../editorBridge";
import { formatDuration } from "../../lib/util";
import { useStore, type TabRun } from "../../store";
import { optimizePrompt } from "../../queryTips";

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

export function TipsBar({ run, tabId }: { run: TabRun; tabId: string }) {
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
