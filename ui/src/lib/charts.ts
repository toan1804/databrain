// Chart kinds beyond bar/line/area (pie, scatter), and charts pinned to
// dashboards: kept by column name so they survive a re-run of the query.

import type { ChartAgg, ChartData, ChartSpec, ColumnMeta } from "./types";

export type ChartKind = "bar" | "line" | "area" | "pie" | "scatter";

/** A chart pinned to a dashboard: its query and how it is drawn. */
export interface ChartPin {
  id: string;
  title: string;
  connection_id: string;
  sql: string;
  kind: ChartKind;
  x: string;
  y: string[];
  agg: ChartAgg;
  series: string | null;
  limit: number;
}

export interface Dashboard {
  id: string;
  name: string;
  charts: ChartPin[];
}

/** A pin's spec for a result with these columns (null when a column is gone). */
export function specFor(pin: ChartPin, columns: ColumnMeta[]): ChartSpec | null {
  const at = (n: string) => columns.findIndex((c) => c.name === n);
  const x = at(pin.x);
  const y = pin.y.map(at);
  const series = pin.series === null ? null : at(pin.series);
  if (x < 0 || y.some((i) => i < 0) || series === -1) return null;
  return { x, y, agg: pin.agg, series, limit: pin.limit };
}

export function pinOf(spec: ChartSpec, kind: ChartKind, columns: ColumnMeta[], base: Pick<ChartPin, "id" | "title" | "connection_id" | "sql">): ChartPin {
  return {
    ...base,
    kind,
    x: columns[spec.x]?.name ?? "",
    y: spec.y.map((i) => columns[i]?.name ?? ""),
    agg: spec.agg,
    series: spec.series == null ? null : (columns[spec.series]?.name ?? null),
    limit: spec.limit ?? 50,
  };
}

/** The spec a chart kind needs: scatter plots raw rows, pie one series. */
export function specForKind(spec: ChartSpec, kind: ChartKind): ChartSpec {
  if (kind === "scatter") return { ...spec, agg: "none", series: null };
  if (kind === "pie") return { ...spec, series: null, agg: spec.agg === "none" ? "sum" : spec.agg, y: spec.y.slice(0, 1) };
  return spec;
}

export interface Slice {
  label: string;
  value: number;
  share: number;
}

/** Pie slices of the first series: the largest `max - 1`, then "Other". Negative and empty values are left out. */
export function pieSlices(data: ChartData, max = 10): Slice[] {
  const s = data.series[0];
  if (!s) return [];
  const items = data.x.map((label, i) => ({ label, value: s.values[i] ?? 0 })).filter((v) => v.value > 0);
  items.sort((a, b) => b.value - a.value);
  const kept = items.length > max ? items.slice(0, max - 1) : items;
  const rest = items.length > max ? items.slice(max - 1).reduce((t, v) => t + v.value, 0) : 0;
  if (rest > 0) kept.push({ label: "Other", value: rest });
  const total = kept.reduce((t, v) => t + v.value, 0) || 1;
  return kept.map((v) => ({ ...v, share: v.value / total }));
}

/** (x, y) points of raw chart data; x as a number or a date. */
export function scatterPoints(data: ChartData): { x: number; y: number; label: string; series: number }[] {
  const asNum = (t: string) => {
    if (data.x_family === "date" || data.x_family === "time") {
      const d = Date.parse(t.includes(" ") && !t.includes("T") ? t.replace(" ", "T") : t);
      return Number.isNaN(d) ? NaN : d;
    }
    return Number(t);
  };
  const out: { x: number; y: number; label: string; series: number }[] = [];
  data.series.forEach((s, si) =>
    s.values.forEach((v, i) => {
      const x = asNum(data.x[i]);
      if (v !== null && Number.isFinite(x)) out.push({ x, y: v, label: data.x[i], series: si });
    }),
  );
  return out;
}
