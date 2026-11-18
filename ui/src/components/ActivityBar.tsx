// A thin bar along the bottom of the window while work runs in the
// background (job runs, in the app or the background runner): what runs and
// how far it got, then how it ended. Part of the layout, so it never covers
// the editor, the results or the assistant; it slides in when work starts
// and away once it is over. × hides it until new work starts or a hidden
// run fails.
import { useEffect, useRef, useState } from "react";
import { CheckCircle2, ExternalLink, Loader2, Square, Workflow, X, XCircle } from "lucide-react";
import { useJobs } from "../jobsStore";
import type { JobRun } from "../lib/types";
import { ProgressBar, runLine } from "./job/shared";

/** A successful (or stopped) run's outcome stays in the bar this long; a failure stays until dismissed. */
export const DONE_MS = 6_000;
/** The bar's height. */
export const BAR_PX = 24;

/** A running job: in the app (with its run) or in the background runner (no run: another process). */
type Running = { key: string; jobId: string; name: string; run?: JobRun };

/** How far a run's running step got (0–1), when it has a total. */
export function runFraction(run: JobRun): number | null {
  const p = run.nodes.find((n) => n.status === "running" && n.progress && n.progress.total > 0)?.progress;
  return p ? Math.min(1, p.done / p.total) : null;
}

export function ActivityBar() {
  const runs = useJobs((s) => s.runs);
  const jobs = useJobs((s) => s.jobs);
  const elsewhere = useJobs((s) => s.elsewhere);
  const seen = useJobs((s) => s.seen);
  const hidden = useJobs((s) => s.barHidden);
  const dismissed = useJobs((s) => s.barDismissed);
  const [open, setOpen] = useState(false);
  const [now, setNow] = useState(Date.now());
  const name = (id: string) => jobs.find((j) => j.id === id)?.name ?? "Job";
  const list = Object.values(runs);
  const here: Running[] = list.filter((r) => r.status === "running" && seen[r.id]).map((r) => ({ key: `run:${r.id}`, jobId: r.job_id, name: name(r.job_id), run: r }));
  const away: Running[] = Object.keys(elsewhere)
    .filter((id) => elsewhere[id] && runs[id]?.status !== "running")
    .map((id) => ({ key: `bg:${id}`, jobId: id, name: name(id) }));
  const running = [...here, ...away].filter((r) => !hidden[r.key]);
  const ended = list.filter((r) => r.status !== "running" && seen[r.id] && !dismissed[r.id]).sort((a, b) => a.started_at - b.started_at);
  const shown = running.length > 0 || ended.length > 0;
  useEffect(() => {
    if (!running.length) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [running.length]);
  useEffect(() => {
    if (!running.length) setOpen(false);
  }, [running.length]);
  const hide = () => {
    const st = useJobs.getState();
    st.hideBar(running.map((r) => r.key));
    for (const r of ended) st.dismissRun(r.id);
    setOpen(false);
  };
  // The running step's progress, as a line along the bar's top edge (one run).
  const fraction = running.length === 1 && running[0].run ? runFraction(running[0].run) : null;
  return (
    <div
      className="relative shrink-0 overflow-visible transition-[height] duration-150 motion-reduce:transition-none"
      style={{ height: shown ? BAR_PX : 0 }}
      aria-hidden={!shown}
    >
      {shown && (
        <div role="region" aria-label="Background activity" className="flex h-full min-w-0 items-center gap-3 border-t border-line bg-panel px-2 text-[11.5px] text-muted">
          {fraction != null && <div className="absolute left-0 top-0 h-[2px] bg-accent transition-[width] duration-200" style={{ width: `${fraction * 100}%` }} aria-hidden />}
          {running.length > 0 && (
            <button className="flex min-w-0 items-center gap-1.5 hover:text-fg" onClick={() => setOpen((o) => !o)} aria-expanded={open} title="Show what runs">
              <Loader2 size={12} className="shrink-0 animate-spin text-accent" />
              <span className="min-w-0 truncate">{summary(running, now)}</span>
            </button>
          )}
          {ended.map((r) => (
            <Outcome key={r.id} run={r} name={name(r.job_id)} />
          ))}
          <span className="flex-1" />
          <button className="shrink-0 rounded p-0.5 hover:bg-hover hover:text-fg" onClick={hide} aria-label="Hide the activity bar" title="Hide until new work starts or a run fails">
            <X size={12} />
          </button>
          {open && running.length > 0 && <Details running={running} now={now} onClose={() => setOpen(false)} />}
        </div>
      )}
    </div>
  );
}

/** One line for what runs: a run's step and progress, or how many run. */
function summary(running: Running[], now: number): string {
  if (running.length === 1) {
    const r = running[0];
    return r.run ? `${r.name} · ${runLine(r.run, now)}` : `${r.name} · running in the background runner`;
  }
  const each = running.map((r) => {
    const f = r.run ? runFraction(r.run) : null;
    return f != null ? `${r.name} ${Math.floor(f * 100)}%` : r.name;
  });
  return `${running.length} jobs running · ${each.join(", ")}`;
}

/** How a run ended: a success or a stop goes after a while, a failure stays until dismissed or opened. */
function Outcome({ run, name }: { run: JobRun; name: string }) {
  const failed = run.status === "error";
  const dismiss = () => useJobs.getState().dismissRun(run.id);
  useEffect(() => {
    if (failed) return;
    const t = setTimeout(() => useJobs.getState().dismissRun(run.id), DONE_MS);
    return () => clearTimeout(t);
  }, [failed, run.id]);
  const line = runLine(run, Date.now());
  const open = () => {
    useJobs.getState().open(run.job_id, name);
    dismiss();
  };
  return (
    <span role={failed ? "alert" : "status"} aria-label={`Job ${name}`} className={`flex min-w-0 max-w-[50%] items-center gap-1 ${failed ? "text-danger" : ""}`}>
      {failed ? (
        <XCircle size={12} className="shrink-0" />
      ) : run.status === "success" ? (
        <CheckCircle2 size={12} className="shrink-0 text-success" />
      ) : (
        <Square size={10} className="shrink-0" />
      )}
      {/* A server's error can be pages long: one line here, all of it on hover and in the job's tab. */}
      <button className="min-w-0 truncate hover:underline" onClick={open} title={`${name}: ${line}`}>
        {name}: {line}
      </button>
      {failed && (
        <button className="shrink-0 rounded p-0.5 hover:bg-hover" onClick={dismiss} aria-label={`Dismiss ${name}`}>
          <X size={11} />
        </button>
      )}
    </span>
  );
}

/** Each running job, above the bar: its step and progress, Open and Stop. */
function Details({ running, now, onClose }: { running: Running[]; now: number; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onDown = (e: MouseEvent) => ref.current && !ref.current.parentElement?.contains(e.target as Node) && onClose();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [onClose]);
  return (
    <div ref={ref} role="dialog" aria-label="Running jobs" className="absolute bottom-full left-2 z-50 mb-1 w-[480px] max-w-[calc(100vw-1rem)] space-y-1 rounded-lg border border-line bg-panel p-1.5 text-fg shadow-xl">
      {running.map((r) => (
        <Row key={r.key} r={r} now={now} />
      ))}
    </div>
  );
}

function Row({ r, now }: { r: Running; now: number }) {
  const [stopping, setStopping] = useState(false);
  const step = r.run?.nodes.find((n) => n.status === "running" && n.progress);
  return (
    <div className="space-y-1 rounded-md px-2 py-1.5 hover:bg-hover/50">
      <div className="flex items-center gap-2 text-[12px]">
        <Workflow size={13} className="shrink-0 text-accent" />
        <span className="min-w-0 flex-1 truncate font-medium">{r.name}</span>
        {/* The background runner's runs (another process) can't be stopped from here. */}
        {r.run && (
          <button
            className="btn-ghost px-1.5 py-0.5 text-[11.5px]"
            disabled={stopping}
            onClick={() => {
              setStopping(true);
              void useJobs.getState().cancel(r.jobId);
            }}
          >
            {stopping ? <Loader2 size={11} className="animate-spin" /> : <Square size={10} />} Stop
          </button>
        )}
        <button className="btn-ghost px-1.5 py-0.5 text-[11.5px]" onClick={() => useJobs.getState().open(r.jobId, r.name)}>
          <ExternalLink size={11} /> Open
        </button>
      </div>
      <div className="line-clamp-2 break-words text-[11px] text-muted" title={r.run ? runLine(r.run, now) : undefined}>
        {stopping ? "Stopping…" : r.run ? runLine(r.run, now) : "Running in the background runner (it stops there, not here)"}
      </div>
      {step?.progress && <ProgressBar p={step.progress} compact />}
    </div>
  );
}
