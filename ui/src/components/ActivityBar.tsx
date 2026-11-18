// A thin bar along the bottom of the window while work runs in the
// background: job runs (in the app or the background runner), full exports
// and "Load into…" loads. What runs and how far it got, then how it ended.
// Part of the layout, so it never covers the editor, the results or the
// assistant; it slides in when work starts and away once it is over. ×
// hides the work running then (per item: new work still shows, a hidden
// item's failure shows, a hidden success is a toast).
import { useEffect, useRef, useState, type ReactNode } from "react";
import { CheckCircle2, Download, ExternalLink, FolderOpen, Loader2, Square, Upload, Workflow, X, XCircle } from "lucide-react";
import { useJobs } from "../jobsStore";
import { phaseLabel, useExports, type BackgroundExport } from "../exportsStore";
import { useLoads, type BackgroundLoad } from "../loadsStore";
import { useActivity } from "../activityStore";
import { useStore } from "../store";
import type { JobRun, StepProgress } from "../lib/types";
import { formatBytes, formatCount, formatDuration } from "../lib/util";
import { ProgressBar, runLine } from "./job/shared";

/** A successful (or stopped) run's or load's outcome stays in the bar this long; a failure stays until dismissed. */
export const DONE_MS = 6_000;
/** The bar's height. */
export const BAR_PX = 24;

/** Something running, whatever it is. */
export interface Live {
  key: string;
  icon: ReactNode;
  title: string;
  /** What it is doing (its step and progress, rows written…). */
  line: string;
  /** In "3 running · …": the title and how far it got. */
  short: string;
  /** 0–1 when the total is known. */
  fraction: number | null;
  /** A job step's progress, for its bar in the details. */
  progress?: StepProgress;
  stopping?: boolean;
  onStop?: () => void;
  onOpen?: () => void;
}

/** How something ended. */
interface Ended {
  key: string;
  /** What it was ("Nightly sales", "Export to all.csv"): its label. */
  name: string;
  failed: boolean;
  /** A success (green) or a stop (grey). */
  ok: boolean;
  text: string;
  /** Click: open what it was (the job, the file). */
  onOpen?: () => void;
  dismiss: () => void;
  /** Goes by itself after this long; none: until dismissed (or its store drops it). */
  autoMs?: number;
}

/** How far a run's running step got (0–1), when it has a total. */
export function runFraction(run: JobRun): number | null {
  const p = run.nodes.find((n) => n.status === "running" && n.progress && n.progress.total > 0)?.progress;
  return p ? Math.min(1, p.done / p.total) : null;
}

const pct = (f: number | null) => (f == null ? "" : ` ${Math.floor(f * 100)}%`);

export function exportLive(x: BackgroundExport, now: number): Live {
  const detail = [x.rows > 0 ? `${formatCount(x.rows)} rows` : "", x.bytes > 0 ? formatBytes(x.bytes) : "", formatDuration(Math.max(0, now - x.started))].filter(Boolean).join(" · ");
  return {
    key: `export:${x.id}`,
    icon: <Download size={13} className="shrink-0 text-accent" />,
    title: `Export to ${x.fileName}`,
    line: `${x.stopping ? "Stopping…" : phaseLabel(x.phase)} ${detail}`,
    short: x.fileName,
    fraction: null,
    stopping: x.stopping,
    onStop: () => useExports.getState().cancel(x.id),
  };
}

export function loadLive(l: BackgroundLoad, now: number): Live {
  const fraction = l.total > 0 ? Math.min(1, l.done / l.total) : null;
  const rows = l.total > 0 ? `${formatCount(l.done)} / ${formatCount(l.total)} rows ·${pct(fraction)}` : `${formatCount(l.done)} rows`;
  return {
    key: `load:${l.id}`,
    icon: <Upload size={13} className="shrink-0 text-accent" />,
    title: `Load into ${l.table} on ${l.target}`,
    line: `${l.stopping ? "Stopping… " : ""}${rows} · ${formatDuration(Math.max(0, now - l.started))}`,
    short: `${l.table}${pct(fraction)}`,
    fraction,
    stopping: l.stopping,
    onStop: () => useLoads.getState().cancel(l.id),
  };
}

export function ActivityBar() {
  const runs = useJobs((s) => s.runs);
  const jobs = useJobs((s) => s.jobs);
  const elsewhere = useJobs((s) => s.elsewhere);
  const seen = useJobs((s) => s.seen);
  const dismissed = useJobs((s) => s.barDismissed);
  const exports = useExports((s) => s.items);
  const exportInDialog = useExports((s) => s.inDialog);
  const loads = useLoads((s) => s.items);
  const loadInDialog = useLoads((s) => s.inDialog);
  const hidden = useActivity((s) => s.hidden);
  const [open, setOpen] = useState(false);
  const shownJob = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId)?.job_id ?? null);
  const [now, setNow] = useState(Date.now());
  const name = (id: string) => jobs.find((j) => j.id === id)?.name ?? "Job";
  const list = Object.values(runs);
  const jobsLive: Live[] = [
    ...list
      .filter((r) => r.status === "running" && seen[r.id])
      .map((r) => {
        const fraction = runFraction(r);
        return {
          key: `run:${r.id}`,
          icon: <Workflow size={13} className="shrink-0 text-accent" />,
          title: name(r.job_id),
          line: runLine(r, now),
          short: `${name(r.job_id)}${pct(fraction)}`,
          fraction,
          progress: r.nodes.find((n) => n.status === "running" && n.progress)?.progress,
          onStop: () => void useJobs.getState().cancel(r.job_id),
          onOpen: () => useJobs.getState().open(r.job_id, name(r.job_id)),
        };
      }),
    // The background runner's (another process): no step, no Stop from here.
    ...Object.keys(elsewhere)
      .filter((id) => elsewhere[id] && runs[id]?.status !== "running")
      .map((id) => ({
        key: `bg:${id}`,
        icon: <Workflow size={13} className="shrink-0 text-accent" />,
        title: name(id),
        line: "Running in the background runner (it stops there, not here)",
        short: name(id),
        fraction: null,
        onOpen: () => useJobs.getState().open(id, name(id)),
      })),
  ];
  // An export or load shown in its dialog is left out: the dialog shows it.
  const running = [
    ...jobsLive,
    ...exports.filter((x) => x.status === "running" && x.id !== exportInDialog).map((x) => exportLive(x, now)),
    ...loads.filter((l) => l.status === "running" && l.id !== loadInDialog).map((l) => loadLive(l, now)),
  ].filter((r) => !hidden[r.key]);
  const ended: Ended[] = [
    ...list
      .filter((r) => r.status !== "running" && seen[r.id] && !dismissed[r.id])
      .sort((a, b) => a.started_at - b.started_at)
      .map((r) => ({
        key: `run:${r.id}`,
        name: `Job ${name(r.job_id)}`,
        failed: r.status === "error",
        ok: r.status === "success",
        text: `${name(r.job_id)}: ${runLine(r, now)}`,
        onOpen: () => {
          useJobs.getState().open(r.job_id, name(r.job_id));
          useJobs.getState().dismissRun(r.id);
        },
        dismiss: () => useJobs.getState().dismissRun(r.id),
        autoMs: r.status === "error" ? undefined : DONE_MS,
      })),
    ...exports
      .filter((x) => x.status !== "running" && x.id !== exportInDialog)
      .map((x) => ({
        key: `export:${x.id}`,
        name: `Export to ${x.fileName}`,
        failed: x.status === "failed",
        ok: x.status === "done",
        text: x.status === "done" ? `Exported ${formatCount(x.rows)} rows to ${x.fileName}` : `Export to ${x.fileName} failed: ${x.error ?? ""}`,
        onOpen: x.status === "done" ? () => useExports.getState().reveal(x.id) : undefined,
        dismiss: () => useExports.getState().dismiss(x.id),
        // A done export goes by its store's clock (time to click it and see the file).
      })),
    ...loads
      .filter((l) => l.status !== "running" && l.id !== loadInDialog)
      .map((l) => ({
        key: `load:${l.id}`,
        name: `Load into ${l.table}`,
        failed: l.status === "failed",
        ok: l.status === "done",
        text: l.status === "done" ? `Loaded ${formatCount(l.done)} rows into ${l.table}` : `Load into ${l.table} failed: ${l.error ?? ""}`,
        dismiss: () => useLoads.getState().dismiss(l.id),
        autoMs: l.status === "done" ? DONE_MS : undefined,
      })),
  ];
  const shown = running.length > 0 || ended.length > 0;
  useEffect(() => {
    if (!running.length) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [running.length]);
  useEffect(() => {
    if (!running.length) setOpen(false);
  }, [running.length]);
  // A run that ended is seen once its job's tab is shown, however it was
  // opened (the bar, the sidebar, the tab bar): its outcome leaves the bar.
  const endedInView = shownJob ? list.find((r) => r.job_id === shownJob && r.status !== "running" && seen[r.id] && !dismissed[r.id])?.id : undefined;
  useEffect(() => {
    if (endedInView != null) useJobs.getState().dismissRun(endedInView);
  }, [endedInView]);
  const hide = () => {
    useActivity.getState().hide(running.map((r) => r.key));
    for (const e of ended) e.dismiss();
    setOpen(false);
  };
  // How far the one running thing got, as a line along the bar's top edge.
  const fraction = running.length === 1 ? running[0].fraction : null;
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
              <span className="min-w-0 truncate">{summary(running)}</span>
            </button>
          )}
          {ended.map((e) => (
            <Outcome key={e.key} e={e} />
          ))}
          <span className="flex-1" />
          <button className="shrink-0 rounded p-0.5 hover:bg-hover hover:text-fg" onClick={hide} aria-label="Hide the activity bar" title="Hide what runs now (new work and failures still show)">
            <X size={12} />
          </button>
          {open && running.length > 0 && <Details running={running} onClose={() => setOpen(false)} />}
        </div>
      )}
    </div>
  );
}

/** One line for what runs: the one thing and how far it got, or how many run. */
function summary(running: Live[]): string {
  if (running.length === 1) return `${running[0].title} · ${running[0].line}`;
  return `${running.length} running · ${running.map((r) => r.short).join(", ")}`;
}

/** How something ended: a success or a stop goes after a while, a failure stays until dismissed or opened. */
function Outcome({ e }: { e: Ended }) {
  const dismiss = useRef(e.dismiss);
  dismiss.current = e.dismiss;
  useEffect(() => {
    if (e.autoMs == null) return;
    const t = setTimeout(() => dismiss.current(), e.autoMs);
    return () => clearTimeout(t);
  }, [e.autoMs, e.key]);
  return (
    <span role={e.failed ? "alert" : "status"} aria-label={e.name} className={`flex min-w-0 max-w-[50%] items-center gap-1 ${e.failed ? "text-danger" : ""}`}>
      {e.failed ? <XCircle size={12} className="shrink-0" /> : e.ok ? <CheckCircle2 size={12} className="shrink-0 text-success" /> : <Square size={10} className="shrink-0" />}
      {/* A server's error can be pages long: one line here, all of it on hover. */}
      {e.onOpen ? (
        <button className="min-w-0 truncate hover:underline" onClick={e.onOpen} title={e.text}>
          {e.text}
        </button>
      ) : (
        <span className="min-w-0 truncate" title={e.text}>
          {e.text}
        </span>
      )}
      {e.key.startsWith("export:") && e.ok && <FolderOpen size={11} className="shrink-0" aria-hidden />}
      {(e.failed || e.autoMs == null) && (
        <button className="shrink-0 rounded p-0.5 hover:bg-hover" onClick={e.dismiss} aria-label={`Dismiss ${e.name}`}>
          <X size={11} />
        </button>
      )}
    </span>
  );
}

/** Each running thing, above the bar: what it does and how far it got, Open and Stop. */
function Details({ running, onClose }: { running: Live[]; onClose: () => void }) {
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
    <div ref={ref} role="dialog" aria-label="Background work" className="absolute bottom-full left-2 z-50 mb-1 w-[480px] max-w-[calc(100vw-1rem)] space-y-1 rounded-lg border border-line bg-panel p-1.5 text-fg shadow-xl">
      {running.map((r) => (
        <Row key={r.key} r={r} />
      ))}
    </div>
  );
}

function Row({ r }: { r: Live }) {
  const [stopping, setStopping] = useState(false);
  const busy = stopping || !!r.stopping;
  return (
    <div className="space-y-1 rounded-md px-2 py-1.5 hover:bg-hover/50">
      <div className="flex items-center gap-2 text-[12px]">
        {r.icon}
        <span className="min-w-0 flex-1 truncate font-medium">{r.title}</span>
        {r.onStop && (
          <button
            className="btn-ghost px-1.5 py-0.5 text-[11.5px]"
            disabled={busy}
            onClick={() => {
              setStopping(true);
              r.onStop!();
            }}
          >
            {busy ? <Loader2 size={11} className="animate-spin" /> : <Square size={10} />} Stop
          </button>
        )}
        {r.onOpen && (
          <button className="btn-ghost px-1.5 py-0.5 text-[11.5px]" onClick={r.onOpen}>
            <ExternalLink size={11} /> Open
          </button>
        )}
      </div>
      <div className="line-clamp-2 break-words text-[11px] text-muted" title={r.line}>
        {busy && !r.line.startsWith("Stopping") ? "Stopping…" : r.line}
      </div>
      {r.progress ? (
        <ProgressBar p={r.progress} compact />
      ) : r.fraction != null ? (
        <div className="h-1 w-full overflow-hidden rounded-full bg-line">
          <div className="h-full rounded-full bg-accent" style={{ width: `${r.fraction * 100}%` }} />
        </div>
      ) : null}
    </div>
  );
}
