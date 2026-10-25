// Chart view for a result: bar / line / area / pie / scatter over chart
// data aggregated in the backend. Pure SVG (no chart library); colors follow
// the theme.

import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, LayoutDashboard, Loader2 } from "lucide-react";
import { api, toError } from "../lib/api";
import { pieSlices, scatterPoints, specForKind, type ChartKind } from "../lib/charts";
import type { ChartAgg, ChartData, ChartSpec, ResultInfo, ViewSpec } from "../lib/types";
import { formatCount } from "../lib/util";

const PALETTE = ["#818cf8", "#34d399", "#fbbf24", "#f87171", "#22d3ee", "#e879f9", "#fb923c", "#a3e635", "#94a3b8", "#f472b6"];
type Kind = ChartKind;
const KINDS: Kind[] = ["bar", "line", "area", "pie", "scatter"];

function defaultSpec(info: ResultInfo): ChartSpec {
  const cols = info.columns;
  const numeric = cols.map((c, i) => (c.family === "number" ? i : -1)).filter((i) => i >= 0);
  const xCandidate = cols.findIndex((c) => c.family === "date" || c.family === "time");
  const text = cols.findIndex((c) => c.family === "text" || c.family === "bool");
  const x = xCandidate >= 0 ? xCandidate : text >= 0 ? text : 0;
  const y = numeric.filter((i) => i !== x).slice(0, 1);
  return { x, y, agg: y.length ? "sum" : "count", series: null, limit: 50 };
}

export function ChartView({
  info,
  view,
  initial,
  compact,
  onPin,
}: {
  info: ResultInfo;
  view: ViewSpec;
  /** Start with this chart (dashboards). */
  initial?: { spec: ChartSpec; kind: Kind };
  /** Only the chart, no settings bar (dashboard cards). */
  compact?: boolean;
  /** "Pin to dashboard" with the chart as drawn. */
  onPin?: (spec: ChartSpec, kind: Kind) => void;
}) {
  const [spec, setSpec] = useState<ChartSpec>(() => initial?.spec ?? defaultSpec(info));
  const [kind, setKind] = useState<Kind>(() => initial?.kind ?? (["date", "time", "number"].includes(info.columns[defaultSpec(info).x]?.family) ? "line" : "bar"));
  const drawn = useMemo(() => specForKind(spec, kind), [spec, kind]);
  const [data, setData] = useState<ChartData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
    const t = setTimeout(() => {
      api
        .chartData(info.id, view, drawn)
        .then((d) => alive && setData(d))
        .catch((e) => alive && setError(toError(e).message))
        .finally(() => alive && setLoading(false));
    }, 120);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [info.id, view, drawn]);

  const cols = info.columns;
  const numericCols = cols.map((c, i) => ({ c, i })).filter(({ c }) => c.family === "number");
  const sel = "h-7 rounded-md border border-line bg-panel-2 px-1.5 text-[12px] outline-none";

  return (
    <div className="flex h-full min-h-0 flex-col">
      {!compact && (
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-2 py-1.5 text-[12px]">
        <div className="flex rounded-md border border-line p-0.5" role="radiogroup" aria-label="Chart type">
          {KINDS.map((k) => (
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
        <select className={sel} aria-label="Aggregation" value={drawn.agg} disabled={spec.y.length === 0 || kind === "scatter"} onChange={(e) => setSpec({ ...spec, agg: e.target.value as ChartAgg })}>
          {(["sum", "avg", "min", "max", "count", "none"] as ChartAgg[]).map((a) => (
            <option key={a} value={a}>
              {a === "none" ? "raw values" : a}
            </option>
          ))}
        </select>
        <label className="flex items-center gap-1 text-muted">
          Split by
          <select className={sel} disabled={kind === "pie" || kind === "scatter"} value={drawn.series ?? -1} onChange={(e) => setSpec({ ...spec, series: Number(e.target.value) < 0 ? null : Number(e.target.value) })}>
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
          {kind === "pie" ? "Max slices" : kind === "scatter" ? "Points" : "Max bars"}
          <select className={sel} value={spec.limit ?? 50} onChange={(e) => setSpec({ ...spec, limit: Number(e.target.value) })}>
            {[10, 25, 50, 100, 250, 500].map((n) => (
              <option key={n} value={n}>
                {kind === "scatter" ? (n * 20).toLocaleString() : n}
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
        {onPin && (
          <button className="btn-ghost ml-auto h-7 border border-line py-0 text-[12px]" title="Add this chart to a dashboard (its query runs again when the dashboard opens)" onClick={() => onPin(drawn, kind)}>
            <LayoutDashboard size={13} /> Pin to dashboard
          </button>
        )}
      </div>
      )}
      <div className="min-h-0 flex-1 p-2">
        {error ? (
          <div className="p-4 text-[12.5px] text-danger">{error}</div>
        ) : data && data.x.length > 0 ? (
          kind === "pie" ? <PieChart data={data} /> : kind === "scatter" ? <ScatterChart data={data} /> : <SvgChart data={data} kind={kind} />
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

function useSize() {
  const host = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 600, h: 300 });
  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => setSize({ w: Math.max(200, e.contentRect.width), h: Math.max(140, e.contentRect.height) }));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return { host, ...size };
}

/** Share of each category in the first series (largest first, the rest as "Other"). */
function PieChart({ data }: { data: ChartData }) {
  const { host, w, h } = useSize();
  const [hover, setHover] = useState<number | null>(null);
  const slices = useMemo(() => pieSlices(data, 10), [data]);
  if (!slices.length) return <div className="p-4 text-[12.5px] text-muted">No positive values to chart</div>;
  const legendW = Math.min(220, w * 0.4);
  const r = Math.max(30, Math.min((w - legendW) / 2, h / 2) - 12);
  const cx = (w - legendW) / 2;
  const cy = h / 2;
  let a0 = -Math.PI / 2;
  const arcs = slices.map((s, i) => {
    const a1 = a0 + s.share * Math.PI * 2;
    const large = a1 - a0 > Math.PI ? 1 : 0;
    const p = (a: number, rr: number) => `${cx + rr * Math.cos(a)},${cy + rr * Math.sin(a)}`;
    const ri = r * 0.55;
    // A full circle can't be one arc: draw it as two halves.
    const d =
      s.share >= 0.9999
        ? `M${p(-Math.PI / 2, r)} A${r},${r} 0 1 1 ${p(Math.PI / 2, r)} A${r},${r} 0 1 1 ${p(-Math.PI / 2, r)} M${p(-Math.PI / 2, ri)} A${ri},${ri} 0 1 0 ${p(Math.PI / 2, ri)} A${ri},${ri} 0 1 0 ${p(-Math.PI / 2, ri)} Z`
        : `M${p(a0, r)} A${r},${r} 0 ${large} 1 ${p(a1, r)} L${p(a1, ri)} A${ri},${ri} 0 ${large} 0 ${p(a0, ri)} Z`;
    a0 = a1;
    return { d, i };
  });
  const cur = hover !== null ? slices[hover] : null;
  return (
    <div ref={host} className="relative h-full w-full" onMouseLeave={() => setHover(null)}>
      <svg width={w} height={h} role="img" aria-label={`Pie chart of ${data.series[0]?.name ?? "values"}`}>
        {arcs.map(({ d, i }) => (
          <path key={i} d={d} fillRule="evenodd" fill={PALETTE[i % PALETTE.length]} opacity={hover === null || hover === i ? 1 : 0.45} onMouseEnter={() => setHover(i)} />
        ))}
        <text x={cx} y={cy - 2} textAnchor="middle" fontSize={13} fill="var(--text)" fontWeight={600}>
          {cur ? `${(cur.share * 100).toFixed(1)}%` : fmt(slices.reduce((t, s) => t + s.value, 0))}
        </text>
        <text x={cx} y={cy + 14} textAnchor="middle" fontSize={10.5} fill="var(--muted)">
          {cur ? (cur.label.length > 18 ? cur.label.slice(0, 17) + "…" : cur.label) : "total"}
        </text>
        {slices.map((s, i) => (
          <g key={i} transform={`translate(${w - legendW + 8}, ${Math.max(8, cy - slices.length * 10) + i * 20})`} onMouseEnter={() => setHover(i)}>
            <rect width={10} height={10} y={1} rx={2} fill={PALETTE[i % PALETTE.length]} />
            <text x={16} y={10} fontSize={11} fill="var(--text)">
              {s.label.length > 16 ? s.label.slice(0, 15) + "…" : s.label}
            </text>
            <text x={legendW - 16} y={10} fontSize={11} textAnchor="end" fill="var(--muted)">
              {(s.share * 100).toFixed(1)}%
            </text>
          </g>
        ))}
      </svg>
    </div>
  );
}

/** One dot per row: x (number or date) against each y column. */
function ScatterChart({ data }: { data: ChartData }) {
  const { host, w, h } = useSize();
  const [hover, setHover] = useState<number | null>(null);
  const pts = useMemo(() => scatterPoints(data), [data]);
  const dates = data.x_family === "date" || data.x_family === "time";
  const [xt, yt, xlo, xhi, ylo, yhi] = useMemo(() => {
    const xs = pts.map((p) => p.x);
    const ys = pts.map((p) => p.y);
    const [x0, x1] = [Math.min(...xs), Math.max(...xs)];
    const [y0, y1] = [Math.min(...ys), Math.max(...ys)];
    const xt = dates ? [] : niceTicks(x0, x1);
    const yt = niceTicks(y0, y1);
    return [xt, yt, dates ? x0 : Math.min(xt[0], x0), dates ? x1 : Math.max(xt[xt.length - 1], x1), Math.min(yt[0], y0), Math.max(yt[yt.length - 1], y1)];
  }, [pts, dates]);
  if (!pts.length) return <div className="p-4 text-[12.5px] text-muted">{dates || data.x_family === "number" ? "No points to chart" : "Scatter needs a number or date column on X"}</div>;
  const legendH = data.series.length > 1 ? 22 : 0;
  const pad = { l: 56, r: 14, t: 10 + legendH, b: 40 };
  const iw = w - pad.l - pad.r;
  const ih = h - pad.t - pad.b;
  const X = (v: number) => pad.l + ((v - xlo) / (xhi - xlo || 1)) * iw;
  const Y = (v: number) => pad.t + ih - ((v - ylo) / (yhi - ylo || 1)) * ih;
  const xLabels = dates ? [0, 0.25, 0.5, 0.75, 1].map((f) => xlo + f * (xhi - xlo)) : xt;
  const xLabel = (v: number) => (dates ? new Date(v).toISOString().slice(0, 10) : fmt(v));
  const p = hover !== null ? pts[hover] : null;
  return (
    <div ref={host} className="relative h-full w-full" onMouseLeave={() => setHover(null)}>
      <svg width={w} height={h} role="img" aria-label="Scatter chart" className="select-none">
        {data.series.length > 1 &&
          data.series.map((s, i) => (
            <g key={s.name} transform={`translate(${pad.l + i * 120}, 4)`}>
              <circle cx={5} cy={5} r={4} fill={PALETTE[i % PALETTE.length]} />
              <text x={14} y={9} fontSize={11} fill="var(--muted)">
                {s.name.length > 14 ? s.name.slice(0, 13) + "…" : s.name}
              </text>
            </g>
          ))}
        {yt.map((t) => (
          <g key={`y${t}`}>
            <line x1={pad.l} x2={w - pad.r} y1={Y(t)} y2={Y(t)} stroke="var(--border)" strokeDasharray="3 3" />
            <text x={pad.l - 6} y={Y(t) + 4} fontSize={10.5} textAnchor="end" fill="var(--muted)">
              {fmt(t)}
            </text>
          </g>
        ))}
        {xLabels.map((t, i) => (
          <text key={`x${i}`} x={X(t)} y={h - pad.b + 16} fontSize={10.5} textAnchor="middle" fill="var(--muted)">
            {xLabel(t)}
          </text>
        ))}
        {pts.map((q, i) => (
          <circle key={i} cx={X(q.x)} cy={Y(q.y)} r={hover === i ? 4.5 : pts.length > 2000 ? 1.8 : 3} fill={PALETTE[q.series % PALETTE.length]} opacity={0.75} onMouseEnter={() => setHover(i)} />
        ))}
      </svg>
      {p && (
        <div className="pointer-events-none absolute z-10 rounded-md border border-line bg-panel px-2 py-1 text-[11.5px] shadow-lg" style={{ left: Math.min(X(p.x) + 10, w - 180), top: Math.max(4, Y(p.y) - 30) }}>
          <div className="font-mono">{p.label}</div>
          <div className="text-muted">
            {data.series[p.series]?.name}: <span className="font-mono text-fg">{fmt(p.y)}</span>
          </div>
        </div>
      )}
    </div>
  );
}
