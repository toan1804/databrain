import { useEffect, useState } from "react";
import { FileCode2, LayoutDashboard, Loader2, RefreshCw, Trash2 } from "lucide-react";
import { pinOf, specFor, type ChartKind, type ChartPin, type Dashboard } from "../lib/charts";
import type { ChartSpec, ResultInfo } from "../lib/types";
import { emptyView, uid } from "../lib/util";
import { useStore } from "../store";
import { ChartView } from "./ChartView";
import { ConnDot, Modal } from "./ui";

/** Rows fetched for a dashboard chart (aggregated in the app). */
const DASH_ROW_LIMIT = 100_000;

const runKey = (dash: string, chart: string) => `dash:${dash}:${chart}`;
const NO_VIEW = emptyView();

/** Ask which dashboard to pin a chart to, then add it. */
export function PinChartDialog({ info, spec, kind, sql, connectionId, defaultTitle, onClose }: { info: ResultInfo; spec: ChartSpec; kind: ChartKind; sql: string; connectionId: string; defaultTitle: string; onClose: () => void }) {
  const dashboards = useStore((s) => s.dashboards);
  const [target, setTarget] = useState<string>(dashboards[0]?.id ?? "__new__");
  const [name, setName] = useState("Dashboard");
  const [title, setTitle] = useState(defaultTitle);
  const pin = () => {
    const st = useStore.getState();
    const chart: ChartPin = pinOf(spec, kind, info.columns, { id: uid(), title: title.trim() || defaultTitle, connection_id: connectionId, sql });
    let list: Dashboard[];
    let id = target;
    if (target === "__new__") {
      id = uid();
      list = [...st.dashboards, { id, name: name.trim() || "Dashboard", charts: [chart] }];
    } else {
      list = st.dashboards.map((d) => (d.id === target ? { ...d, charts: [...d.charts, chart] } : d));
    }
    st.saveDashboards(list);
    st.toast(`Pinned to ${list.find((d) => d.id === id)?.name}`, "success");
    onClose();
    st.openDashboard(id);
  };
  return (
    <Modal
      title="Pin chart to a dashboard"
      onClose={onClose}
      footer={
        <>
          <button className="btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button className="btn-primary" onClick={pin}>
            Pin
          </button>
        </>
      }
    >
      <div className="flex flex-col gap-3 text-[12.5px]">
        <label className="flex flex-col gap-1">
          <span className="text-muted">Chart title</span>
          <input className="field" value={title} onChange={(e) => setTitle(e.target.value)} autoFocus />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-muted">Dashboard</span>
          <select className="field" value={target} onChange={(e) => setTarget(e.target.value)}>
            {dashboards.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name} ({d.charts.length})
              </option>
            ))}
            <option value="__new__">New dashboard…</option>
          </select>
        </label>
        {target === "__new__" && (
          <label className="flex flex-col gap-1">
            <span className="text-muted">Name</span>
            <input className="field" value={name} onChange={(e) => setName(e.target.value)} />
          </label>
        )}
        <p className="text-[11.5px] text-muted">The chart keeps its query: opening the dashboard runs it again (up to {DASH_ROW_LIMIT.toLocaleString()} rows) for current numbers.</p>
      </div>
    </Modal>
  );
}

export function DashboardTab({ tabId, visible }: { tabId: string; visible: boolean }) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const dash = useStore((s) => s.dashboards.find((d) => d.id === tab?.dashboard_id));
  const [loaded, setLoaded] = useState(false);

  const refresh = (c: ChartPin) => {
    if (!dash) return;
    // Each chart on a session of its own: they refresh at the same time.
    void useStore.getState().runSql(runKey(dash.id, c.id), c.connection_id, c.sql, 0, undefined, null, {}, DASH_ROW_LIMIT);
  };
  const refreshAll = () => dash?.charts.forEach(refresh);
  // Run every chart's query the first time the tab is shown.
  useEffect(() => {
    if (visible && dash && !loaded) {
      setLoaded(true);
      refreshAll();
    }
  }, [visible, dash?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  if (!tab || !dash) return visible ? <p className="p-4 text-[12.5px] text-muted">This dashboard was deleted.</p> : null;
  const save = (d: Dashboard) => useStore.getState().saveDashboards(useStore.getState().dashboards.map((x) => (x.id === d.id ? d : x)));
  const rename = () => {
    const name = window.prompt("Dashboard name", dash.name)?.trim();
    if (name) {
      save({ ...dash, name });
      useStore.getState().updateTab(tab.id, { title: name });
    }
  };
  const remove = () => {
    if (window.confirm(`Delete the dashboard "${dash.name}"? Its charts' queries are not affected.`)) useStore.getState().saveDashboards(useStore.getState().dashboards.filter((d) => d.id !== dash.id));
  };

  return (
    <div className="flex h-full min-h-0 flex-col" style={{ display: visible ? "flex" : "none" }}>
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-line px-3 text-[12.5px]">
        <LayoutDashboard size={14} className="text-muted" />
        <button className="font-medium hover:underline" title="Rename" onClick={rename}>
          {dash.name}
        </button>
        <span className="text-muted">
          {dash.charts.length} chart{dash.charts.length === 1 ? "" : "s"}
        </span>
        <button className="btn-ghost ml-auto h-7 py-0 text-[12px]" onClick={refreshAll}>
          <RefreshCw size={13} /> Refresh all
        </button>
        <button className="icon-btn" title="Delete dashboard" aria-label="Delete dashboard" onClick={remove}>
          <Trash2 size={14} />
        </button>
      </div>
      {dash.charts.length === 0 ? (
        <p className="p-4 text-[12.5px] text-muted">No charts yet: open a result's Chart view and use “Pin to dashboard”.</p>
      ) : (
        <div className="grid min-h-0 flex-1 auto-rows-[320px] grid-cols-1 gap-3 overflow-auto p-3 xl:grid-cols-2">
          {dash.charts.map((c) => (
            <ChartCard key={c.id} dash={dash} chart={c} onRefresh={() => refresh(c)} onRemove={() => save({ ...dash, charts: dash.charts.filter((x) => x.id !== c.id) })} />
          ))}
        </div>
      )}
    </div>
  );
}

function ChartCard({ dash, chart, onRefresh, onRemove }: { dash: Dashboard; chart: ChartPin; onRefresh: () => void; onRemove: () => void }) {
  const run = useStore((s) => s.runs[runKey(dash.id, chart.id)]);
  const conn = useStore((s) => s.connections.find((c) => c.id === chart.connection_id));
  const last = run?.statements.filter((s) => s.result).at(-1);
  const failed = run?.statements.find((s) => s.error);
  const info = last?.result;
  const spec = info ? specFor(chart, info.columns) : null;
  const openQuery = () => {
    const st = useStore.getState();
    const id = st.newTab({ title: chart.title, sql: chart.sql, connection_id: chart.connection_id });
    st.setActiveTab(id);
  };
  return (
    <section className="flex min-h-0 flex-col rounded-lg border border-line bg-panel" aria-label={chart.title}>
      <header className="flex h-8 shrink-0 items-center gap-1.5 border-b border-line px-2 text-[12px]">
        <span className="min-w-0 flex-1 truncate font-medium" title={chart.sql}>
          {chart.title}
        </span>
        {conn && (
          <span className="flex shrink-0 items-center gap-1 text-[11px] text-muted">
            <ConnDot color={conn.color} connected={conn.connected} /> {conn.name}
          </span>
        )}
        {run?.running && <Loader2 size={12} className="animate-spin text-muted" />}
        <button className="icon-btn h-6 w-6" title="Run the query again" aria-label="Refresh chart" disabled={run?.running} onClick={onRefresh}>
          <RefreshCw size={12} />
        </button>
        <button className="icon-btn h-6 w-6" title="Open the query in a tab" aria-label="Open query" onClick={openQuery}>
          <FileCode2 size={12} />
        </button>
        <button className="icon-btn h-6 w-6" title="Remove from the dashboard" aria-label="Remove chart" onClick={onRemove}>
          <Trash2 size={12} />
        </button>
      </header>
      <div className="min-h-0 flex-1">
        {!conn ? (
          <p className="p-3 text-[12px] text-danger">Its connection no longer exists.</p>
        ) : failed?.error ? (
          <p className="p-3 text-[12px] text-danger">{failed.error.message}</p>
        ) : info && spec ? (
          <ChartView key={info.id} info={info} view={NO_VIEW} initial={{ spec, kind: chart.kind }} compact />
        ) : info ? (
          <p className="p-3 text-[12px] text-warning">The query no longer returns the columns this chart uses ({[chart.x, ...chart.y].join(", ")}).</p>
        ) : (
          <p className="flex items-center gap-2 p-3 text-[12px] text-muted">
            <Loader2 size={12} className="animate-spin" /> Running…
          </p>
        )}
      </div>
    </section>
  );
}
