// Pieces the job editor's parts share: status icon and text, progress bar,
// step lists, field styles.
import { FileDown, ArrowDownToLine, CheckCircle2, CircleDashed, Database, Loader2, XCircle } from "lucide-react";
import type { JobNode, NodeRunSummary, StepProgress } from "../../lib/types";
import { formatCount, formatDuration, relativeTime } from "../../lib/util";
import { useStore } from "../../store";
import { flushDrafts } from "../../drafts";

export const EDITOR_KEY = (job: string, node: string) => `job:${job}:${node}`;

export const isResults = (c: { config: { options?: Partial<Record<string, string>> } }) => c.config.options?.databrain_results === "1";

/** `t` with the text just typed in its editor. */
export function latestTab<T extends { id: string }>(t: T): T {
  flushDrafts();
  return (useStore.getState().tabs.find((x) => x.id === t.id) as T | undefined) ?? t;
}

export function StatusIcon({ status }: { status?: NodeRunSummary["status"] }) {
  switch (status) {
    case "running":
      return <Loader2 size={12} className="shrink-0 animate-spin text-accent" />;
    case "success":
      return <CheckCircle2 size={12} className="shrink-0 text-success" />;
    case "error":
      return <XCircle size={12} className="shrink-0 text-danger" />;
    case "skipped":
    case "cancelled":
    case "pending":
      return <CircleDashed size={12} className="shrink-0 text-muted" />;
    default:
      return null;
  }
}

export function statusText(s: NodeRunSummary | null): string {
  if (!s || !s.status) return "Not run yet";
  switch (s.status) {
    case "running":
      return s.progress ? `${s.progress.phase}: ${formatCount(s.progress.done)} / ${formatCount(s.progress.total)}` : "Running…";
    case "pending":
      return "Waiting for upstream steps";
    case "skipped":
      return "Skipped: an upstream step failed";
    case "cancelled":
      return "Cancelled";
    case "error":
      return s.error ?? "Failed";
    default:
      return `${s.rows != null ? `${formatCount(s.rows)} rows · ` : ""}${s.file ? `${s.file.split(/[/\\]/).pop()} · ` : ""}${formatDuration(s.duration_ms)}${s.finished_at ? ` · ${relativeTime(s.finished_at)}` : ""}`;
  }
}

export const field = "w-full rounded-md border border-line bg-bg px-2 py-1 text-[12px] focus:border-accent focus:outline-none";
export const label = "mb-1 block text-[11px] font-medium uppercase tracking-wide text-muted";

/** Rows written so far by a running load step. */
export function ProgressBar({ p, compact = false }: { p: StepProgress; compact?: boolean }) {
  const pct = p.total > 0 ? Math.min(100, (p.done / p.total) * 100) : 100;
  return (
    <div className={compact ? "" : "space-y-0.5"}>
      <div
        className={`w-full overflow-hidden rounded-full bg-line ${compact ? "h-1" : "h-1.5"}`}
        role="progressbar"
        aria-label={p.phase}
        aria-valuemin={0}
        aria-valuemax={p.total}
        aria-valuenow={p.done}
      >
        <div className="h-full rounded-full bg-accent transition-[width] duration-200" style={{ width: `${pct}%` }} />
      </div>
      {!compact && (
        <div className="flex justify-between text-[11px] text-muted">
          <span>{p.phase}</span>
          <span>
            {formatCount(p.done)} / {formatCount(p.total)} rows · {Math.floor(pct)}%
          </span>
        </div>
      )}
    </div>
  );
}

export function actionIcon(kind: JobNode["kind"], size = 14) {
  return kind === "load" ? <ArrowDownToLine size={size} className="shrink-0 text-accent" /> : kind === "export" ? <FileDown size={size} className="shrink-0 text-accent" /> : <Database size={size} className="shrink-0 text-accent" />;
}

export function NodeList({ title, nodes, onSelect, empty }: { title: string; nodes: JobNode[]; onSelect: (id: string) => void; empty: string }) {
  return (
    <div>
      <div className={label}>{title}</div>
      {nodes.length === 0 ? (
        <div className="text-muted">{empty}</div>
      ) : (
        nodes.map((n) => (
          <button key={n.id} className="block max-w-full truncate font-mono text-accent hover:underline" onClick={() => onSelect(n.id)}>
            {n.name}
          </button>
        ))
      )}
    </div>
  );
}
