import { SelectionSummaryBar, type GridRange } from "../SelectionTools";
import type { ResultInfo, ViewSpec } from "../../lib/types";
import { formatBytes, formatCount, formatDuration } from "../../lib/util";
import type { StatementRun } from "../../store";

/** Status line under the result: rows (of total when filtered), limit, columns, time, selection, size. */
export function ResultFooter({
  stmt,
  info,
  view,
  mode,
  filtered,
  viewRows,
  selRange,
}: {
  stmt: Pick<StatementRun, "durationMs" | "output">;
  info: ResultInfo;
  view: ViewSpec;
  mode: "grid" | "chart";
  filtered: boolean;
  viewRows: number;
  selRange: GridRange | null;
}) {
  return (
  <div className="flex h-7 shrink-0 items-center gap-3 border-t border-line px-3 text-[11.5px] text-muted">
    <span>
      {filtered ? `${formatCount(viewRows)} of ${formatCount(info.total_rows)} rows` : `${formatCount(info.total_rows)} rows`}
    </span>
    {info.truncated && (
      <span className="rounded bg-warning/15 px-1.5 text-warning" title="Increase the row limit in the editor toolbar to fetch more">
        limited to {formatCount(info.total_rows)}
      </span>
    )}
    <span>{info.columns.length} columns</span>
    {stmt.durationMs !== undefined && <span>{formatDuration(stmt.durationMs)}</span>}
    <span className="ml-auto" />
    {mode === "grid" && <SelectionSummaryBar info={info} view={view} range={selRange} />}
    <span>{formatBytes(info.bytes)}</span>
  </div>
  );
}
