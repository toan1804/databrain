import { useEffect, useState } from "react";
import { ArrowDown, ArrowUp, BarChart3, Loader2 } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { ColumnStats, FilterOp, ResultInfo, ViewSpec } from "../../lib/types";
import { FILTER_OPS, formatCount, upsertFilter } from "../../lib/util";
import { Popover } from "../ui";

export function ColumnMenu({
  info,
  col,
  x,
  y,
  view,
  onView,
  onClose,
}: {
  info: ResultInfo;
  col: number;
  x: number;
  y: number;
  view: ViewSpec;
  onView: (v: ViewSpec) => void;
  onClose: () => void;
}) {
  const meta = info.columns[col];
  const existing = view.filters.find((f) => f.column === col);
  const [op, setOp] = useState<FilterOp>(existing?.op ?? (meta.family === "number" ? "equals" : "contains"));
  const [value, setValue] = useState(existing?.value ?? "");
  const [stats, setStats] = useState<ColumnStats | null>(null);
  const [statsError, setStatsError] = useState<string | null>(null);
  const needsValue = FILTER_OPS.find((o) => o.op === op)?.needsValue ?? true;

  useEffect(() => {
    let alive = true;
    api
      .columnStats(info.id, view, col)
      .then((s) => alive && setStats(s))
      .catch((e) => alive && setStatsError(toError(e).message));
    return () => {
      alive = false;
    };
  }, [info.id, view, col]);

  const apply = () => {
    onView({ ...view, filters: upsertFilter(view.filters.filter((f) => f.column !== col), { column: col, op, value }) });
    onClose();
  };

  const maxTop = stats?.top[0]?.count ?? 1;

  return (
    <Popover x={x} y={y} onClose={onClose} className="w-72 p-0">
      <div className="border-b border-line px-3 py-2">
        <div className="truncate font-semibold">{meta.name}</div>
        <div className="font-mono text-[11px] text-muted">{meta.db_type ?? meta.data_type}</div>
      </div>
      <div className="flex gap-1 border-b border-line p-1.5">
        <button className="btn-ghost flex-1 justify-center py-1" onClick={() => { onView({ ...view, sort: [{ column: col, descending: false }] }); onClose(); }}>
          <ArrowUp size={13} /> Asc
        </button>
        <button className="btn-ghost flex-1 justify-center py-1" onClick={() => { onView({ ...view, sort: [{ column: col, descending: true }] }); onClose(); }}>
          <ArrowDown size={13} /> Desc
        </button>
      </div>
      <form
        className="space-y-1.5 border-b border-line p-2"
        onSubmit={(e) => {
          e.preventDefault();
          apply();
        }}
      >
        <div className="text-[11px] font-medium text-muted">Filter</div>
        <select className="field py-1" value={op} aria-label="Filter operator" onChange={(e) => setOp(e.target.value as FilterOp)}>
          {FILTER_OPS.map((o) => (
            <option key={o.op} value={o.op}>
              {o.label}
            </option>
          ))}
        </select>
        {needsValue && (
          <input className="field py-1" autoFocus value={value} aria-label="Filter value" placeholder="Value" onChange={(e) => setValue(e.target.value)} />
        )}
        <div className="flex justify-end gap-1 pt-0.5">
          {existing && (
            <button
              type="button"
              className="btn-ghost py-1"
              onClick={() => {
                onView({ ...view, filters: view.filters.filter((f) => f.column !== col) });
                onClose();
              }}
            >
              Remove
            </button>
          )}
          <button type="submit" className="btn-primary py-1">
            Apply
          </button>
        </div>
      </form>
      <div className="p-2">
        <div className="mb-1 flex items-center gap-1 text-[11px] font-medium text-muted">
          <BarChart3 size={11} /> Statistics
        </div>
        {statsError && <div className="text-[11.5px] text-danger">{statsError}</div>}
        {!stats && !statsError && <Loader2 size={13} className="animate-spin text-muted" />}
        {stats && (
          <div className="space-y-1.5 text-[11.5px]">
            <div className="grid grid-cols-3 gap-1">
              <Stat label="Rows" value={formatCount(stats.count)} />
              <Stat label="Distinct" value={formatCount(stats.distinct)} />
              <Stat label="Nulls" value={formatCount(stats.nulls)} />
            </div>
            {stats.min !== null && (
              <div className="grid grid-cols-2 gap-1">
                <Stat label="Min" value={stats.min} />
                <Stat label="Max" value={stats.max ?? ""} />
              </div>
            )}
            <div className="max-h-40 space-y-0.5 overflow-auto">
              {stats.top.map((t, i) => (
                <button
                  key={i}
                  className="relative flex w-full items-center justify-between overflow-hidden rounded px-1.5 py-0.5 text-left hover:bg-hover"
                  title="Filter by this value"
                  onClick={() => {
                    onView({
                      ...view,
                      filters: upsertFilter(view.filters.filter((f) => f.column !== col), {
                        column: col,
                        op: t.value === null ? "is_null" : "equals",
                        value: t.value ?? "",
                      }),
                    });
                    onClose();
                  }}
                >
                  <span className="absolute inset-y-0 left-0 bg-accent/12" style={{ width: `${(t.count / maxTop) * 100}%` }} />
                  <span className={`relative truncate font-mono ${t.value === null ? "italic text-muted" : ""}`}>{t.value ?? "NULL"}</span>
                  <span className="relative ml-2 text-muted">{formatCount(t.count)}</span>
                </button>
              ))}
            </div>
          </div>
        )}
      </div>
    </Popover>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0 rounded bg-panel-2 px-1.5 py-1">
      <div className="text-[10px] text-muted">{label}</div>
      <div className="truncate font-mono" title={value}>
        {value}
      </div>
    </div>
  );
}
