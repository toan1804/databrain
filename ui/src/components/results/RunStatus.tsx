import { useEffect, useState } from "react";
import { AlertCircle, Ban, CheckCircle2, Loader2, Wrench, X } from "lucide-react";
import { lineOf, locateStatement, useAi } from "../../aiStore";
import { editorBridge } from "../../editorBridge";
import { formatCount, formatDuration, sqlPreview } from "../../lib/util";
import { useStore, type StatementRun, type TabRun } from "../../store";
import { kbd } from "../../lib/platform";

export function StatusIcon({ s }: { s: StatementRun }) {
  if (s.status === "running" || s.status === "pending") return <Loader2 size={12} className="animate-spin text-accent" />;
  if (s.status === "error") return <AlertCircle size={12} className="text-danger" />;
  if (s.status === "cancelled") return <Ban size={12} className="text-muted" />;
  return <CheckCircle2 size={12} className="text-success" />;
}

export function RunSummary({ run }: { run: TabRun }) {
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

export function RunningView({ run, tabId }: { run: TabRun; tabId: string }) {
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
        <X size={14} /> Cancel <span className="kbd ml-1">{kbd("⌘.")}</span>
      </button>
    </div>
  );
}

export function ErrorView({ stmt, tabId, connectionId, count }: { stmt: StatementRun; tabId: string; connectionId?: string | null; count: number }) {
  const e = stmt.error!;
  const cancelled = stmt.status === "cancelled";
  const send = useAi((s) => s.send);
  // Line in the editor, so "statement 1" isn't read as the editor's first one
  // when only a later statement was run.
  const doc = editorBridge.snapshot(tabId)?.sql;
  const at = doc !== undefined ? locateStatement(doc, stmt.plan) : null;
  const line = doc !== undefined && at ? lineOf(doc, at.from) : null;
  const fix = () =>
    void send({
      message: `Fix this error in the statement that failed${line ? ` (line ${line})` : ""}:\n${e.message}`,
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
          {cancelled
            ? "Query cancelled"
            : `${count > 1 ? `Statement ${stmt.plan.index + 1} of ${count}` : "Statement"} failed${line ? ` · line ${line}` : ""}`}
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

export function DoneView({ stmt }: { stmt: StatementRun }) {
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

export function Messages({ run }: { run: TabRun }) {
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
