// A job run's log: live from events while it runs, else read from the app.
import { useEffect, useRef, useState } from "react";
import { Copy } from "lucide-react";
import { api } from "../../lib/api";
import type { Job, JobRun, RunLogEntry } from "../../lib/types";
import { formatDuration } from "../../lib/util";
import { useStore } from "../../store";
import { useJobs } from "../../jobsStore";

/** Log of a run: live from events while it runs, else read from the app. */
function useRunLog(jobId: string, run: JobRun | undefined): { entries: RunLogEntry[]; loading: boolean } {
  const live = useJobs((s) => (run ? s.logs[run.id] : undefined));
  const [saved, setSaved] = useState<{ id: number; entries: RunLogEntry[] } | null>(null);
  const running = run?.status === "running";
  useEffect(() => {
    if (!run || running) return;
    let ok = true;
    api
      .jobRunLog(jobId, run.id)
      .then((entries) => ok && setSaved({ id: run.id, entries }))
      .catch(() => ok && setSaved({ id: run.id, entries: [] }));
    return () => {
      ok = false;
    };
  }, [jobId, run?.id, running]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!run) return { entries: [], loading: false };
  if (saved?.id === run.id && !running) return { entries: saved.entries, loading: false };
  return { entries: live ?? [], loading: !running && !live };
}

const LEVEL_TONE: Record<RunLogEntry["level"], string> = { info: "text-muted", success: "text-success", warning: "text-warning", error: "text-danger" };

function logText(entries: RunLogEntry[]): string {
  return entries
    .map((e) => {
      const head = `${new Date(e.at).toLocaleTimeString()} ${e.level.toUpperCase().padEnd(7)} ${e.step ? `[${e.step}] ` : ""}${e.message}${e.duration_ms != null ? ` (${formatDuration(e.duration_ms)})` : ""}`;
      return e.sql ? `${head}\n    ${e.sql.replace(/\n/g, "\n    ")}` : head;
    })
    .join("\n");
}

/** A run's log: what ran on which step, SQL, rows, errors. */
export function RunLog({ job, run, step, onSelect }: { job: Job; run: JobRun | undefined; step?: string; onSelect?: (id: string) => void }) {
  // One step's log sits above its settings: kept short there.
  const maxH = step ? "max-h-48" : "max-h-80";
  const { entries, loading } = useRunLog(job.id, run);
  const [filter, setFilter] = useState<string>(step ?? "");
  const [onlyProblems, setOnlyProblems] = useState(false);
  const [openSql, setOpenSql] = useState<Set<number>>(new Set());
  const end = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  useEffect(() => setFilter(step ?? ""), [step]);
  const shown = entries.filter((e) => (!filter || e.node_id === filter || (!e.node_id && !step)) && (!onlyProblems || e.level === "error" || e.level === "warning"));
  useEffect(() => {
    if (run?.status === "running" && follow.current) end.current?.scrollIntoView({ block: "nearest" });
  }, [shown.length, run?.status]);
  if (!run) return <div className="text-[11.5px] text-muted">No run yet.</div>;
  const steps = job.nodes.filter((n) => entries.some((e) => e.node_id === n.id));
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-1.5">
        {!step && (
          <select className="rounded-md border border-line bg-bg px-1.5 py-0.5 text-[11.5px]" value={filter} onChange={(e) => setFilter(e.target.value)} aria-label="Show the log of">
            <option value="">All steps</option>
            {steps.map((n) => (
              <option key={n.id} value={n.id}>
                {n.name}
              </option>
            ))}
          </select>
        )}
        <label className="flex items-center gap-1 text-[11.5px]">
          <input type="checkbox" checked={onlyProblems} onChange={(e) => setOnlyProblems(e.target.checked)} /> Problems only
        </label>
        <button
          className="btn-ghost ml-auto py-0 text-[11px]"
          disabled={!shown.length}
          onClick={() => void navigator.clipboard.writeText(logText(shown)).then(() => useStore.getState().toast("Log copied", "info"))}
        >
          <Copy size={11} /> Copy
        </button>
      </div>
      <div
        className={`${maxH} overflow-y-auto rounded-md border border-line bg-bg px-1.5 py-1 font-mono text-[11px] leading-[1.45]`}
        role="log"
        aria-live={run.status === "running" ? "polite" : "off"}
        onScroll={(e) => {
          const el = e.currentTarget;
          follow.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
        }}
      >
        {loading && <div className="text-muted">Loading…</div>}
        {!loading && !shown.length && <div className="text-muted">{entries.length ? "Nothing matches." : run.status === "running" ? "Starting…" : "No log for this run (runs before logs were kept have none)."}</div>}
        {shown.map((e) => (
          <div key={e.seq} className="py-px">
            <div className="flex items-baseline gap-1.5">
              <span className="shrink-0 text-muted">{new Date(e.at).toLocaleTimeString()}</span>
              {e.step && !step && (
                <button className="shrink-0 text-accent hover:underline" onClick={() => e.node_id && onSelect?.(e.node_id)}>
                  {e.step}
                </button>
              )}
              <span className={`min-w-0 flex-1 whitespace-pre-wrap break-words ${LEVEL_TONE[e.level] === "text-muted" ? "text-fg" : LEVEL_TONE[e.level]}`}>{e.message}</span>
              {e.duration_ms != null && <span className="shrink-0 text-muted">{formatDuration(e.duration_ms)}</span>}
              {e.sql && (
                <button
                  className="shrink-0 text-muted hover:text-fg"
                  aria-expanded={openSql.has(e.seq)}
                  onClick={() =>
                    setOpenSql((s) => {
                      const n = new Set(s);
                      if (n.has(e.seq)) n.delete(e.seq);
                      else n.add(e.seq);
                      return n;
                    })
                  }
                >
                  SQL
                </button>
              )}
            </div>
            {e.sql && openSql.has(e.seq) && <pre className="ml-14 mt-0.5 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded bg-panel-2 px-1.5 py-1 text-[10.5px]">{e.sql}</pre>}
          </div>
        ))}
        <div ref={end} />
      </div>
    </div>
  );
}
