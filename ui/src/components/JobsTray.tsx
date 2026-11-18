import { useEffect, useState } from "react";
import { CheckCircle2, ExternalLink, Loader2, Square, Workflow, X, XCircle } from "lucide-react";
import { useJobs } from "../jobsStore";
import { useStore } from "../store";
import type { JobRun } from "../lib/types";
import { ProgressBar, runLine } from "./job/shared";

/** A finished run's card goes away by itself after this long (failed and stopped ones stay). */
export const DONE_CARD_MS = 15_000;

/**
 * Jobs running while their tab isn't the one shown (bottom right, with the
 * exports and toasts): which step runs and how far it got, then how the run
 * ended, so a run of half an hour isn't silent while the user works in an
 * editor. A run whose tab is shown has no card: the tab shows it.
 */
export function JobsTray() {
  const runs = useJobs((s) => s.runs);
  const jobs = useJobs((s) => s.jobs);
  const elsewhere = useJobs((s) => s.elsewhere);
  const here = useJobs((s) => s.running);
  const shownJob = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId)?.job_id ?? null);
  const [dismissed, setDismissed] = useState<ReadonlySet<number>>(new Set());
  // Runs seen running in this session: a finished run gets a card only if
  // it was (not the last run of every job when the app starts).
  const [seen, setSeen] = useState<ReadonlySet<number>>(new Set());
  const [now, setNow] = useState(Date.now());
  const list = Object.values(runs);
  const runningKey = list
    .filter((r) => r.status === "running")
    .map((r) => r.id)
    .join(",");
  useEffect(() => {
    if (!runningKey) return;
    setSeen((s) => {
      const ids = runningKey.split(",").map(Number);
      return ids.every((id) => s.has(id)) ? s : new Set([...s, ...ids]);
    });
  }, [runningKey]);
  const anyRunning = runningKey !== "";
  useEffect(() => {
    if (!anyRunning) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [anyRunning]);
  const dismiss = (id: number) => setDismissed((d) => new Set(d).add(id));
  // A run shown in its tab (while it runs or once it ended) needs no card later.
  useEffect(() => {
    const r = shownJob ? runs[shownJob] : undefined;
    if (r && r.status !== "running" && !dismissed.has(r.id)) setDismissed((d) => new Set(d).add(r.id));
  }, [shownJob, runs, dismissed]);
  // A successful run's card goes after a while.
  const doneIds = list.filter((r) => r.status === "success" && seen.has(r.id) && !dismissed.has(r.id)).map((r) => r.id);
  const doneKey = doneIds.join(",");
  useEffect(() => {
    if (!doneKey) return;
    const ids = doneKey.split(",").map(Number);
    const t = setTimeout(() => setDismissed((d) => new Set([...d, ...ids])), DONE_CARD_MS);
    return () => clearTimeout(t);
  }, [doneKey]);
  const cards = list
    .filter((r) => r.job_id !== shownJob && seen.has(r.id) && !dismissed.has(r.id))
    .sort((a, b) => a.started_at - b.started_at);
  if (cards.length === 0) return null;
  return (
    <>
      {cards.map((r) => (
        <JobCard
          key={r.id}
          run={r}
          name={jobs.find((j) => j.id === r.job_id)?.name ?? "Job"}
          now={now}
          // The background runner's runs (another process) can't be stopped from here.
          stoppable={!elsewhere[r.job_id] || !!here[r.job_id]}
          onDismiss={() => dismiss(r.id)}
        />
      ))}
    </>
  );
}

function JobCard({ run, name, now, stoppable, onDismiss }: { run: JobRun; name: string; now: number; stoppable: boolean; onDismiss: () => void }) {
  const [stopping, setStopping] = useState(false);
  const running = run.status === "running";
  const step = running ? run.nodes.find((n) => n.status === "running" && n.progress) : undefined;
  const open = () => {
    useJobs.getState().open(run.job_id, name);
    if (!running) onDismiss();
  };
  const stop = () => {
    setStopping(true);
    void useJobs.getState().cancel(run.job_id);
  };
  return (
    <div
      role={run.status === "error" ? "alert" : "status"}
      aria-label={`Job ${name}`}
      className="pointer-events-auto space-y-1.5 rounded-lg border border-line bg-panel px-3 py-2.5 shadow-lg"
    >
      <div className="flex items-start gap-2">
        {running ? (
          <Workflow size={16} className="mt-px shrink-0 text-accent" />
        ) : run.status === "success" ? (
          <CheckCircle2 size={16} className="mt-px shrink-0 text-success" />
        ) : run.status === "cancelled" ? (
          <Square size={14} className="mt-px shrink-0 text-muted" />
        ) : (
          <XCircle size={16} className="mt-px shrink-0 text-danger" />
        )}
        <div className="min-w-0 flex-1 text-[12.5px]">
          <div className="truncate font-medium">{name}</div>
          <div className={`text-[11.5px] ${run.status === "error" ? "break-words text-danger" : "truncate text-muted"}`} title={runLine(run, now)}>
            {stopping && running ? "Stopping…" : runLine(run, now)}
          </div>
        </div>
        {!running && (
          <button className="text-muted hover:text-fg" onClick={onDismiss} aria-label="Dismiss">
            <X size={14} />
          </button>
        )}
      </div>
      {step?.progress && <ProgressBar p={step.progress} compact />}
      <div className="flex justify-end gap-1">
        {running && stoppable && (
          <button className="btn-ghost px-2 py-0.5 text-[12px]" onClick={stop} disabled={stopping}>
            {stopping ? <Loader2 size={12} className="animate-spin" /> : <Square size={11} />} Stop
          </button>
        )}
        <button className="btn-ghost px-2 py-0.5 text-[12px]" onClick={open}>
          <ExternalLink size={12} /> Open
        </button>
      </div>
    </div>
  );
}
