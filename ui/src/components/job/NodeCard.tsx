// A step on the job canvas: name, action, connection, status, link handle.
import { type PointerEvent as ReactPointerEvent } from "react";
import { FileDown, ArrowDownToLine, Database, Square } from "lucide-react";
import type { JobNode, NodeRunSummary } from "../../lib/types";
import { NODE_H, NODE_W } from "../../lib/jobGraph";
import { useStore } from "../../store";
import { ConnDot } from "../ui";
import { folderName } from "./ExportFields";
import { ProgressBar, StatusIcon, isResults, statusText } from "./shared";

export function NodeCard({
  node,
  status,
  selected,
  dim,
  role,
  linking,
  onPointerDown,
  onPortDown,
  onStop,
  onContextMenu,
}: {
  node: JobNode;
  onStop: () => void;
  status: NodeRunSummary | null;
  selected: boolean;
  dim: boolean;
  role?: "upstream" | "downstream";
  linking: boolean;
  onPointerDown: (e: ReactPointerEvent) => void;
  onPortDown: (e: ReactPointerEvent) => void;
  onContextMenu: (e: React.MouseEvent) => void;
}) {
  const conn = useStore((s) => s.connections.find((c) => c.id === node.connection_id));
  const target = useStore((s) => s.connections.find((c) => c.id === node.target_connection_id));
  const tone =
    status?.status === "error" ? "border-danger/70" : status?.status === "running" ? "border-accent" : status?.status === "success" ? "border-success/60" : "border-line";
  return (
    <div
      data-node={node.id}
      className={`absolute flex cursor-pointer select-none flex-col rounded-lg border bg-panel px-2.5 py-1.5 shadow-sm transition-opacity ${tone} ${
        selected ? "ring-2 ring-accent" : linking ? "hover:ring-2 hover:ring-accent/60" : ""
      } ${dim ? "opacity-50" : ""}`}
      style={{ left: node.x, top: node.y, width: NODE_W, height: NODE_H }}
      onPointerDown={onPointerDown}
      onContextMenu={onContextMenu}
    >
      <span className="absolute -top-[5px] left-1/2 h-2.5 w-2.5 -translate-x-1/2 rounded-full border border-line bg-panel-2" aria-hidden />
      <div className="flex items-center gap-1.5">
        {node.kind === "load" ? (
          <ArrowDownToLine size={12} className="shrink-0 text-accent" />
        ) : node.kind === "export" ? (
          <FileDown size={12} className="shrink-0 text-accent" />
        ) : (
          <Database size={12} className="shrink-0 text-muted" />
        )}
        <span className="min-w-0 flex-1 truncate font-mono text-[12px] font-semibold">{node.name}</span>
        {role && <span className="shrink-0 rounded bg-accent/10 px-1 text-[9.5px] uppercase text-accent">{role}</span>}
      </div>
      <div className="mt-0.5 flex min-w-0 items-center gap-1 text-[10.5px] text-muted">
        {node.kind === "export" ? (
          <span className="truncate" title={node.export_folder || "Downloads"}>
            DuckDB → {node.export_file?.trim() || `${node.name}_…`}
            {node.export_file?.toLowerCase().endsWith(`.${node.export_format ?? "csv"}`) ? "" : `.${node.export_format ?? "csv"}`} in {folderName(node.export_folder)}
          </span>
        ) : node.kind === "load" ? (
          <span className="truncate">
            DuckDB → {target?.name ?? "choose a connection"}
            {node.target_table ? `.${node.target_table}` : ""}
          </span>
        ) : conn && !isResults(conn) ? (
          <>
            <ConnDot color={conn.color} />
            <span className="truncate">{conn.name}</span>
          </>
        ) : (
          <span className="truncate">Results (DuckDB)</span>
        )}
      </div>
      {status?.status === "running" && status.progress && (
        <div className="mt-1">
          <ProgressBar p={status.progress} compact />
        </div>
      )}
      <div className={`mt-auto flex min-w-0 items-center gap-1 text-[10.5px] ${status?.status === "error" ? "text-danger" : "text-muted"}`} title={status?.error ?? undefined}>
        <StatusIcon status={status?.status} />
        <span className="min-w-0 flex-1 truncate">{node.sql.trim() || node.kind !== "query" ? statusText(status) : "No SQL yet"}</span>
        {status?.status === "running" && (
          <button
            type="button"
            className="shrink-0 rounded px-1 text-danger hover:bg-danger/10"
            aria-label={`Stop ${node.name}`}
            title="Stop this step"
            onPointerDown={(e) => e.stopPropagation()}
            onClick={(e) => {
              e.stopPropagation();
              onStop();
            }}
          >
            <Square size={10} />
          </button>
        )}
      </div>
      <span
        data-port
        role="button"
        aria-label={`Link ${node.name} to a downstream step`}
        title="Drag onto a step to run it after this one (or onto empty space to add one)"
        className="absolute -bottom-[7px] left-1/2 h-3.5 w-3.5 -translate-x-1/2 cursor-crosshair rounded-full border-2 border-accent bg-panel hover:scale-125"
        onPointerDown={onPortDown}
      />
    </div>
  );
}
