// Chart view for a result: bar / line / area over aggregated chart data.
// Pure SVG (no chart library); colors follow the theme.

import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Loader2 } from "lucide-react";
import { api, toError } from "../lib/api";
import type { ChartAgg, ChartData, ChartSpec, ResultInfo, ViewSpec } from "../lib/types";
import { formatCount } from "../lib/util";

const PALETTE = ["#818cf8", "#34d399", "#fbbf24", "#f87171", "#22d3ee", "#e879f9", "#fb923c", "#a3e635", "#94a3b8", "#f472b6"];
type Kind = "bar" | "line" | "area";

function defaultSpec(info: ResultInfo): ChartSpec {
  const cols = info.columns;
  const numeric = cols.map((c, i) => (c.family === "number" ? i : -1)).filter((i) => i >= 0);
  const xCandidate = cols.findIndex((c) => c.family === "date" || c.family === "time");
  const text = cols.findIndex((c) => c.family === "text" || c.family === "bool");
  const x = xCandidate >= 0 ? xCandidate : text >= 0 ? text : 0;
  const y = numeric.filter((i) => i !== x).slice(0, 1);
  return { x, y, agg: y.length ? "sum" : "count", series: null, limit: 50 };
}

export function ChartView({ info, view }: { info: ResultInfo; view: ViewSpec }) {
  const [spec, setSpec] = useState<ChartSpec>(() => defaultSpec(info));
  const [kind, setKind] = useState<Kind>(() => (["date", "time", "number"].includes(info.columns[defaultSpec(info).x]?.family) ? "line" : "bar"));
  const [data, setData] = useState<ChartData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
    const t = setTimeout(() => {
      api
        .chartData(info.id, view, spec)
        .then((d) => alive && setData(d))
        .catch((e) => alive && setError(toError(e).message))
        .finally(() => alive && setLoading(false));
    }, 120);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [info.id, view, spec]);

  const cols = info.columns;
  const numericCols = cols.map((c, i) => ({ c, i })).filter(({ c }) => c.family === "number");
  const sel = "h-7 rounded-md border border-line bg-panel-2 px-1.5 text-[12px] outline-none";

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-2 py-1.5 text-[12px]">
        <div className="flex rounded-md border border-line p-0.5" role="radiogroup" aria-label="Chart type">
          {(["bar", "line", "area"] as Kind[]).map((k) => (
            <button
              key={k}
              role="radio"
              aria-checked={kind === k}
              onClick={() => setKind(k)}
              className={`rounded px-2 py-0.5 capitalize ${kind === k ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
            >
              {k}
            </button>
          ))}
        </div>
        <label className="flex items-center gap-1 text-muted">
          X
          <select className={sel} value={spec.x} onChange={(e) => setSpec({ ...spec, x: Number(e.target.value) })}>
            {cols.map((c, i) => (
              <option key={i} value={i}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-1 text-muted">
          Y
          <select
            className={sel}
            value={spec.y[0] ?? -1}
            onChange={(e) => {
              const v = Number(e.target.value);
              setSpec({ ...spec, y: v < 0 ? [] : [v], agg: v < 0 ? "count" : spec.agg === "count" ? "sum" : spec.agg });
            }}
          >
            <option value={-1}>(row count)</option>
            {numericCols.map(({ c, i }) => (
              <option key={i} value={i}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
        <select className={sel} aria-label="Aggregation" value={spec.agg} disabled={spec.y.length === 0} onChange={(e) => setSpec({ ...spec, agg: e.target.value as ChartAgg })}>
          {(["sum", "avg", "min", "max", "count", "none"] as ChartAgg[]).map((a) => (
            <option key={a} value={a}>
              {a === "none" ? "raw values" : a}
            </option>
          ))}
        </select>
        <label className="flex items-center gap-1 text-muted">
          Split by
          <select className={sel} value={spec.series ?? -1} onChange={(e) => setSpec({ ...spec, series: Number(e.target.value) < 0 ? null : Number(e.target.value) })}>
            <option value={-1}>—</option>
            {cols.map((c, i) =>
              i === spec.x ? null : (
                <option key={i} value={i}>
                  {c.name}
                </option>
              ),
            )}
          </select>
        </label>
        <label className="flex items-center gap-1 text-muted">
          Max bars
          <select className={sel} value={spec.limit ?? 50} onChange={(e) => setSpec({ ...spec, limit: Number(e.target.value) })}>
            {[10, 25, 50, 100, 250, 500].map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </label>
        {loading && <Loader2 size={13} className="animate-spin text-muted" />}
        {data?.truncated && (
          <span className="flex items-center gap-1 rounded bg-warning/15 px-1.5 text-[11px] text-warning" title="Only the largest categories / first points are drawn">
            <AlertTriangle size={11} /> limited
          </span>
        )}
      </div>
      <div className="min-h-0 flex-1 p-2">
        {error ? (
          <div className="p-4 text-[12.5px] text-danger">{error}</div>
        ) : data && data.x.length > 0 ? (
          <SvgChart data={data} kind={kind} />
        ) : data ? (
          <div className="p-4 text-[12.5px] text-muted">No data to chart</div>
        ) : null}
      </div>
    </div>
  );
}

function niceTicks(min: number, max: number, count = 5): number[] {
  if (!isFinite(min) || !isFinite(max)) return [0];
  if (min === max) {
    min = min === 0 ? 0 : Math.min(0, min);
    max = max === 0 ? 1 : max;
  }
  const span = max - min;
  const step0 = span / count;
  const mag = 10 ** Math.floor(Math.log10(step0));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => span / s <= count) ?? 10 * mag;
  const start = Math.floor(min / step) * step;
  const out: number[] = [];
  for (let v = start; v <= max + step * 0.5; v += step) out.push(Number(v.toPrecision(12)));
  return out;
}

function fmt(v: number): string {
  const a = Math.abs(v);
  if (a >= 1e9) return `${(v / 1e9).toFixed(1)}B`;
  if (a >= 1e6) return `${(v / 1e6).toFixed(1)}M`;
  if (a >= 1e4) return `${(v / 1e3).toFixed(1)}K`;
  return Number.isInteger(v) ? formatCount(v) : v.toFixed(a < 1 ? 3 : 2);
}

function SvgChart({ data, kind }: { data: ChartData; kind: Kind }) {
  const host = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 600, h: 300 });
  const [hover, setHover] = useState<number | null>(null);
  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => setSize({ w: Math.max(200, e.contentRect.width), h: Math.max(140, e.contentRect.height) }));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const { ticks, lo, hi } = useMemo(() => {
    const vals = data.series.flatMap((s) => s.values.filter((v): v is number => v !== null));
    const min = Math.min(0, ...vals);
    const max = Math.max(0, ...vals);
    const t = niceTicks(min, max);
    return { ticks: t, lo: Math.min(t[0], min), hi: Math.max(t[t.length - 1], max) };
  }, [data]);

  const legendH = data.series.length > 1 ? 22 : 0;
  const pad = { l: 56, r: 12, t: 10 + legendH, b: 40 };
  const W = size.w;
  const H = size.h;
  const iw = W - pad.l - pad.r;
  const ih = H - pad.t - pad.b;
  const n = data.x.length;
  const y = (v: number) => pad.t + ih - ((v - lo) / (hi - lo || 1)) * ih;
  const band = iw / n;
  const xc = (i: number) => pad.l + band * i + band / 2;
  const labelEvery = Math.max(1, Math.ceil((n * 70) / iw));
  const groupW = Math.min(band * 0.8, 60);
  const barW = groupW / data.series.length;

  return (
    <div ref={host} className="relative h-full w-full" onMouseLeave={() => setHover(null)}>
      <svg width={W} height={H} role="img" aria-label="Chart" className="select-none">
        {data.series.length > 1 &&
          data.series.map((s, i) => (
            <g key={s.name} transform={`translate(${pad.l + i * 120}, 4)`}>
              <rect width={10} height={10} rx={2} fill={PALETTE[i % PALETTE.length]} />
              <text x={14} y={9} fontSize={11} fill="var(--muted)">
                {s.name.length > 14 ? s.name.slice(0, 13) + "…" : s.name}
              </text>
            </g>
          ))}
        {ticks.map((t) => (
          <g key={t}>
            <line x1={pad.l} x2={W - pad.r} y1={y(t)} y2={y(t)} stroke="var(--border)" strokeDasharray={t === 0 ? undefined : "3 3"} />
            <text x={pad.l - 6} y={y(t) + 4} fontSize={10.5} textAnchor="end" fill="var(--muted)">
              {fmt(t)}
            </text>
          </g>
        ))}
        {data.x.map((label, i) =>
          i % labelEvery === 0 ? (
            <text key={i} x={xc(i)} y={H - pad.b + 16} fontSize={10.5} textAnchor="middle" fill="var(--muted)">
              {label.length > 12 ? label.slice(0, 11) + "…" : label}
            </text>
          ) : null,
        )}
        {kind === "bar"
          ? data.series.map((s, si) =>
              s.values.map((v, i) =>
                v === null ? null : (
                  <rect
                    key={`${si}-${i}`}
                    x={xc(i) - groupW / 2 + si * barW}
                    y={Math.min(y(v), y(0))}
                    width={Math.max(1, barW - 1)}
                    height={Math.max(1, Math.abs(y(v) - y(0)))}
                    rx={2}
                    fill={PALETTE[si % PALETTE.length]}
                    opacity={hover === null || hover === i ? 1 : 0.55}
                  />
                ),
              ),
            )
          : data.series.map((s, si) => {
              const pts = s.values.map((v, i) => (v === null ? null : [xc(i), y(v)] as const)).filter((p): p is readonly [number, number] => p !== null);
              if (pts.length === 0) return null;
              const d = pts.map(([px, py], i) => `${i ? "L" : "M"}${px},${py}`).join(" ");
              const color = PALETTE[si % PALETTE.length];
              return (
                <g key={si}>
                  {kind === "area" && <path d={`${d} L${pts[pts.length - 1][0]},${y(0)} L${pts[0][0]},${y(0)} Z`} fill={color} opacity={0.18} />}
                  <path d={d} fill="none" stroke={color} strokeWidth={2} />
                  {pts.length <= 120 && pts.map(([px, py], i) => <circle key={i} cx={px} cy={py} r={2.5} fill={color} />)}
                </g>
              );
            })}
        {/* hover bands */}
        {data.x.map((_, i) => (
          <rect key={`h${i}`} x={pad.l + band * i} y={pad.t} width={band} height={ih} fill="transparent" onMouseEnter={() => setHover(i)} />
        ))}
        {hover !== null && kind !== "bar" && <line x1={xc(hover)} x2={xc(hover)} y1={pad.t} y2={pad.t + ih} stroke="var(--muted)" strokeDasharray="2 2" />}
      </svg>
      {hover !== null && (
        <div
          className="pointer-events-none absolute z-10 rounded-md border border-line bg-panel px-2 py-1 text-[11.5px] shadow-lg"
          style={{ left: Math.min(xc(hover) + 10, W - 180), top: pad.t + 4 }}
        >
          <div className="mb-0.5 font-medium">{data.x[hover]}</div>
          {data.series.map((s, si) => (
            <div key={si} className="flex items-center gap-1.5">
              <span className="inline-block h-2 w-2 rounded-sm" style={{ background: PALETTE[si % PALETTE.length] }} />
              <span className="text-muted">{s.name}</span>
              <span className="ml-auto font-mono">{s.values[hover] === null ? "—" : fmt(s.values[hover]!)}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
