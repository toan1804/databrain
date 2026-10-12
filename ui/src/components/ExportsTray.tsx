import { useEffect, useState } from "react";
import { CheckCircle2, Download, FolderOpen, Loader2, Square, X, XCircle } from "lucide-react";
import { phaseLabel, useExports, type BackgroundExport } from "../exportsStore";
import { formatBytes, formatCount, formatDuration } from "../lib/util";

/** Full exports running in the background (bottom right, above the toasts). */
export function ExportsTray() {
  const all = useExports((s) => s.items);
  const inDialog = useExports((s) => s.inDialog);
  const items = all.filter((x) => x.id !== inDialog);
  const running = items.some((x) => x.status === "running");
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [running]);
  if (items.length === 0) return null;
  return (
    <>
      {items.map((x) => (
        <ExportCard key={x.id} x={x} now={now} />
      ))}
    </>
  );
}

export function ExportCard({ x, now, bare = false }: { x: BackgroundExport; now: number; bare?: boolean }) {
  const { cancel, dismiss, reveal } = useExports.getState();
  const elapsed = formatDuration(Math.max(0, (x.finished ?? now) - x.started));
  const rows = x.rows > 0 ? `${formatCount(x.rows)} rows` : "";
  const size = x.bytes > 0 ? formatBytes(x.bytes) : "";
  const detail = [rows, size, elapsed].filter(Boolean).join(" · ");
  return (
    <div
      role={x.status === "failed" ? "alert" : "status"}
      aria-label={`Export ${x.fileName}`}
      className={bare ? "space-y-1.5" : "pointer-events-auto space-y-1.5 rounded-lg border border-line bg-panel px-3 py-2.5 shadow-lg"}
    >
      <div className="flex items-start gap-2">
        {x.status === "running" ? (
          <Download size={16} className="mt-px shrink-0 text-accent" />
        ) : x.status === "done" ? (
          <CheckCircle2 size={16} className="mt-px shrink-0 text-success" />
        ) : (
          <XCircle size={16} className="mt-px shrink-0 text-danger" />
        )}
        <div className="min-w-0 flex-1 text-[12.5px]">
          <div className="truncate font-medium" title={x.path}>
            {x.fileName}
          </div>
          <div className="truncate text-[11.5px] text-muted">
            {x.status === "running"
              ? `${x.stopping ? "Stopping…" : phaseLabel(x.phase)}${detail ? ` ${detail}` : ""}`
              : x.status === "done"
                ? `Exported ${detail}`
                : "Export failed"}
          </div>
        </div>
        {!bare && x.status !== "running" && (
          <button className="text-muted hover:text-fg" onClick={() => dismiss(x.id)} aria-label="Dismiss">
            <X size={14} />
          </button>
        )}
      </div>
      {x.status === "running" && <IndeterminateBar label={`Exporting ${x.fileName}`} />}
      {x.status === "failed" && x.error && <div className="whitespace-pre-wrap break-words text-[11.5px] text-danger">{x.error}</div>}
      {!bare && (x.status === "running" || x.status === "done") && (
        <div className="flex justify-end gap-1">
          {x.status === "running" ? (
            <button className="btn-ghost px-2 py-0.5 text-[12px]" onClick={() => cancel(x.id)} disabled={x.stopping}>
              {x.stopping ? <Loader2 size={12} className="animate-spin" /> : <Square size={11} />} Stop
            </button>
          ) : (
            <button className="btn-ghost px-2 py-0.5 text-[12px]" onClick={() => reveal(x.id)}>
              <FolderOpen size={12} /> Show file
            </button>
          )}
        </div>
      )}
    </div>
  );
}

/** The total is not known until the query ends, so the bar only shows activity. */
function IndeterminateBar({ label }: { label: string }) {
  return (
    <div className="relative h-1 w-full overflow-hidden rounded-full bg-line" role="progressbar" aria-label={label} aria-busy="true">
      <div className="progress-indeterminate absolute inset-y-0 left-0 w-1/3 rounded-full bg-accent" />
    </div>
  );
}
