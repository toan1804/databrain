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
  windowRows = null,
  selRange,
}: {
  stmt: Pick<StatementRun, "durationMs" | "output">;
  info: ResultInfo;
  view: ViewSpec;
  mode: "grid" | "chart";
  filtered: boolean;
  viewRows: number;
  /** A sort or filter of a result on disk applies to its first rows only: this many. */
  windowRows?: number | null;
  selRange: GridRange | null;
}) {
  return (
  <div className="flex h-7 shrink-0 items-center gap-3 border-t border-line px-3 text-[11.5px] text-muted">
    {windowRows !== null ? (
      <span
        className="rounded bg-warning/15 px-1.5 text-warning"
        title={`Sorting and filtering a result on disk apply to the rows loaded in the grid (the first ${formatCount(windowRows)}), not to all ${formatCount(info.total_rows)}. To include more, clear the sort and filters and scroll further first, or query results.<name> with SQL.`}
      >
        {filtered ? `${formatCount(viewRows)} of the first ${formatCount(windowRows)} rows` : `first ${formatCount(windowRows)} of ${formatCount(info.total_rows)} rows sorted`}
      </span>
    ) : (
      <span>{filtered ? `${formatCount(viewRows)} of ${formatCount(info.total_rows)} rows` : `${formatCount(info.total_rows)} rows`}</span>
    )}
    {info.truncated && (
      <span className="rounded bg-warning/15 px-1.5 text-warning" title="Increase the row limit in the editor toolbar to fetch more">
        limited to {formatCount(info.total_rows)}
      </span>
    )}
    {info.on_disk && (
      <span
        className="rounded bg-hover px-1.5"
        title="Larger than the memory a result may keep: its rows are on disk and read as you scroll. Sorting and filtering apply to the rows loaded in the grid; find, statistics and export of the whole result read every row from disk."
      >
        on disk · {formatBytes(info.disk_bytes)}
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
