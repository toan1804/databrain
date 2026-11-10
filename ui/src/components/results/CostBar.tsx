import { AlertTriangle, Gauge, Loader2, RefreshCw, X } from "lucide-react";
import { estimateCost, useCost } from "../../costPreview";
import { costLevel, costSummary } from "../../lib/cost";
import { sqlPreview } from "../../lib/util";

/** The cost preview of the tab's statement, next to the slow-query tips. */
export function CostBar({ tabId }: { tabId: string }) {
  const s = useCost((c) => c.byTab[tabId]);
  const hidden = useCost((c) => c.hidden[tabId]);
  const hide = useCost((c) => c.hide);
  if (!s || hidden === s.sql) return null;
  // A typing-started check that failed (an unfinished statement) stays quiet.
  const quiet = s.status === "error" && s.auto;
  const warn = s.status === "ready" && costLevel(s.estimate) === "warn";
  return (
    <div
      className={`flex shrink-0 items-start gap-1.5 border-b border-line px-2 py-1 text-[12px] ${warn ? "bg-warning/5" : s.status === "error" && !quiet ? "bg-danger/5" : "bg-accent/5"}`}
      role="region"
      aria-label="Cost preview"
      aria-live="polite"
    >
      {s.status === "loading" ? (
        <Loader2 size={13} className="mt-0.5 shrink-0 animate-spin text-muted" />
      ) : warn || (s.status === "error" && !quiet) ? (
        <AlertTriangle size={13} className={`mt-0.5 shrink-0 ${warn ? "text-warning" : "text-danger"}`} />
      ) : (
        <Gauge size={13} className="mt-0.5 shrink-0 text-accent" />
      )}
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 items-baseline gap-1.5">
          <span className="shrink-0 font-medium">Cost preview</span>
          {s.status === "loading" && <span className="text-muted">estimating…</span>}
          {s.status === "ready" && (
            <>
              <span className={warn ? "text-warning" : ""}>{costSummary(s.estimate)}</span>
              <span className="shrink-0 text-[11px] text-muted">({s.estimate.method}, nothing ran)</span>
            </>
          )}
          {s.status === "error" && <span className={`min-w-0 truncate ${quiet ? "text-muted" : "text-danger"}`} title={s.message}>Can't estimate: {s.message}</span>}
          <span className="min-w-0 truncate font-mono text-[11px] text-muted" title={s.sql}>
            {sqlPreview(s.sql, 80)}
          </span>
        </div>
        {s.status === "ready" && s.estimate.notes.length > 0 && (
          <ul className="text-[11.5px] text-muted">
            {s.estimate.notes.map((n) => (
              <li key={n}>{n}</li>
            ))}
          </ul>
        )}
      </div>
      <button className="icon-btn h-5 w-5 shrink-0" aria-label="Estimate again" title="Estimate the statement at the cursor again" onClick={() => void estimateCost(tabId)}>
        <RefreshCw size={12} />
      </button>
      <button className="icon-btn h-5 w-5 shrink-0" aria-label="Hide cost preview" title="Hide until the statement changes" onClick={() => hide(tabId, s.sql)}>
        <X size={12} />
      </button>
    </div>
  );
}
