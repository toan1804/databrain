import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, ClipboardCopy, Flame, GitFork, Loader2, RefreshCw } from "lucide-react";
import { api, toError } from "../lib/api";
import { flattenPlan, fmtMs, fmtNum, hottest, type PlanRow } from "../lib/plan";
import type { QueryPlan } from "../lib/types";
import { sqlPreview } from "../lib/util";
import { isQueryTab, useStore, type PlanRef } from "../store";
import { editorBridge } from "../editorBridge";
import { copyText } from "./CatalogMenus";
import { ConnDot } from "./ui";

/** Engines with EXPLAIN (and ANALYZE) DataBrain reads; mirrors the backend's plan_support. */
const PLAN_KINDS: Record<string, { analyze: boolean }> = {
  postgres: { analyze: true },
  mysql: { analyze: true },
  duckdb: { analyze: true },
  sqlite: { analyze: false },
  mssql: { analyze: false },
  oracle: { analyze: false },
  snowflake: { analyze: false },
  databricks: { analyze: false },
};

/**
 * Open the plan of the editor's selection, or of the statement at the
 * cursor, in a tab of its own (reused for the same editor).
 */
export async function openPlan(tabId: string, analyze = false) {
  const st = useStore.getState();
  const tab = st.tabs.find((t) => t.id === tabId);
  if (!isQueryTab(tab)) return;
  const conn = st.connections.find((c) => c.id === tab.connection_id);
  if (!conn) return st.toast("Choose a connection for this tab first", "error");
  const support = PLAN_KINDS[conn.config.kind];
  if (!support) return st.toast(`Query plans aren't available for this database`, "info");
  const view = editorBridge.get(tabId);
  const doc = view?.state.doc.toString() ?? tab.sql;
  const sel = view?.state.selection.main;
  let sql = sel && !sel.empty ? doc.slice(sel.from, sel.to) : "";
  if (!sql.trim()) {
    const span = await api.statementAtCursor(conn.config.kind, doc, sel?.head ?? 0).catch(() => null);
    sql = span?.sql ?? "";
  }
  if (!sql.trim()) return st.toast("Nothing to explain: put the cursor in a statement", "info");
  const params = await st.paramsFor(tabId, conn.config.kind, sql);
  if (!params) return;
  const plan: PlanRef = { source: tabId, analyze: analyze && support.analyze, params };
  const existing = st.tabs.find((t) => t.plan?.source === tabId);
  if (existing) {
    st.updateTab(existing.id, { sql, plan, connection_id: conn.id });
    return st.setActiveTab(existing.id);
  }
  st.newTab({ title: `Plan · ${tab.title}`, sql, connection_id: conn.id, plan });
}

type State = { status: "loading" } | { status: "ready"; plan: QueryPlan } | { status: "error"; message: string };

export function PlanTab({ tabId, visible }: { tabId: string; visible: boolean }) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const conn = useStore((s) => s.connections.find((c) => c.id === tab?.connection_id));
  const updateTab = useStore((s) => s.updateTab);
  const ref = tab?.plan;
  const [state, setState] = useState<State>({ status: "loading" });
  const [view, setView] = useState<"tree" | "raw">("tree");
  const [picked, setPicked] = useState<string | null>(null);
  const seq = useRef(0);

  const load = useCallback(async () => {
    if (!tab || !ref) return;
    const my = ++seq.current;
    setState({ status: "loading" });
    if (!conn) return setState({ status: "error", message: "The connection of this tab no longer exists." });
    try {
      const plan = await api.explainPlan(conn.id, ref.source, tab.sql, ref.analyze, ref.params);
      if (my === seq.current) setState({ status: "ready", plan });
    } catch (e) {
      if (my === seq.current) setState({ status: "error", message: toError(e).message });
    }
    // Reload when the statement, mode or connection changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab?.sql, ref?.analyze, ref?.source, conn?.id]);

  useEffect(() => {
    void load();
  }, [load]);

  const rows = useMemo(() => (state.status === "ready" ? flattenPlan(state.plan.roots) : []), [state]);
  const hot = useMemo(() => hottest(rows), [rows]);
  const maxCost = useMemo(() => Math.max(0, ...rows.map((r) => r.node.cost ?? 0)), [rows]);
  const total = useMemo(() => Math.max(0, ...rows.filter((r) => r.depth === 0).map((r) => r.totalMs ?? 0)), [rows]);
  const sel = rows.find((r) => r.id === picked) ?? null;
  const warnCount = rows.reduce((n, r) => n + r.warnings.length, 0);
  const canAnalyze = !!conn && !!PLAN_KINDS[conn.config.kind]?.analyze;

  if (!tab || !ref) return null;
  const setAnalyze = (analyze: boolean) => updateTab(tab.id, { plan: { ...ref, analyze } });

  return (
    <div className="flex h-full min-h-0 flex-col" style={{ display: visible ? "flex" : "none" }}>
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-line px-2 text-[12px]">
        <ConnDot color={conn?.color} connected={conn?.connected} />
        <span className="shrink-0 text-muted">{conn?.name ?? "No connection"}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-[11.5px] text-muted" title={tab.sql}>
          {sqlPreview(tab.sql, 160)}
        </span>
        <div className="flex shrink-0 rounded-md border border-line p-0.5" role="radiogroup" aria-label="Plan kind">
          {([false, true] as const).map((a) => (
            <button
              key={String(a)}
              role="radio"
              aria-checked={ref.analyze === a}
              disabled={a && !canAnalyze}
              title={a ? (canAnalyze ? "Run the query and measure each step (EXPLAIN ANALYZE; read queries only)" : "This database's plans can't be analyzed here") : "The planner's estimate; nothing runs"}
              onClick={() => setAnalyze(a)}
              className={`h-5 rounded px-2 text-[11.5px] disabled:opacity-40 ${ref.analyze === a ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
            >
              {a ? "Analyze" : "Estimate"}
            </button>
          ))}
        </div>
        <div className="flex shrink-0 rounded-md border border-line p-0.5" role="radiogroup" aria-label="View">
          {(["tree", "raw"] as const).map((v) => (
            <button key={v} role="radio" aria-checked={view === v} onClick={() => setView(v)} className={`h-5 rounded px-2 text-[11.5px] ${view === v ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}>
              {v === "tree" ? "Tree" : "Raw"}
            </button>
          ))}
        </div>
        <button className="icon-btn" title="Copy the plan as the database printed it" aria-label="Copy plan" disabled={state.status !== "ready"} onClick={() => state.status === "ready" && void copyText(state.plan.raw, "plan")}>
          <ClipboardCopy size={14} />
        </button>
        <button className="icon-btn" title="Explain again" aria-label="Refresh plan" onClick={() => void load()}>
          <RefreshCw size={14} />
        </button>
      </div>

      {state.status === "loading" ? (
        <div className="flex flex-1 items-center justify-center gap-2 text-muted">
          <Loader2 size={16} className="animate-spin" /> {ref.analyze ? "Running the query to measure it…" : "Asking the planner…"}
        </div>
      ) : state.status === "error" ? (
        <div className="p-4 text-[12.5px] text-danger" role="alert">
          {state.message}
        </div>
      ) : view === "raw" || state.plan.roots.length === 0 ? (
        <pre className="min-h-0 flex-1 overflow-auto p-3 font-mono text-[11.5px]">{state.plan.raw || "The database returned no plan."}</pre>
      ) : (
        <>
          <div className="flex h-8 shrink-0 items-center gap-4 border-b border-line bg-panel-2 px-3 text-[11.5px] text-muted" aria-live="polite">
            <span>{rows.length} steps</span>
            {state.plan.planning_ms !== undefined && <span>planning {fmtMs(state.plan.planning_ms)}</span>}
            {(state.plan.execution_ms ?? (total || undefined)) !== undefined && <span>execution {fmtMs(state.plan.execution_ms ?? total)}</span>}
            {hot && (
              <button className="flex items-center gap-1 text-warning hover:underline" onClick={() => setPicked(hot.id)}>
                <Flame size={12} /> slowest: {hot.node.op} ({fmtMs(hot.selfMs ?? 0)})
              </button>
            )}
            {warnCount > 0 && (
              <span className="flex items-center gap-1 text-warning">
                <AlertTriangle size={12} /> {warnCount} warning{warnCount === 1 ? "" : "s"}
              </span>
            )}
            {!state.plan.analyzed && canAnalyze && <span>Estimates only: Analyze shows real rows and times</span>}
          </div>
          <div className="flex min-h-0 flex-1">
            <div className="min-h-0 flex-1 overflow-auto" role="tree" aria-label="Query plan">
              <table className="w-full border-collapse text-[12px]">
                <thead className="sticky top-0 bg-panel text-[11px] text-muted">
                  <tr className="border-b border-line">
                    <th className="px-2 py-1 text-left font-normal">Step</th>
                    <th className="px-2 py-1 text-right font-normal">Est. rows</th>
                    {state.plan.analyzed && <th className="px-2 py-1 text-right font-normal">Rows</th>}
                    {state.plan.analyzed && <th className="px-2 py-1 text-right font-normal">Loops</th>}
                    <th className="px-2 py-1 text-right font-normal">Cost</th>
                    {state.plan.analyzed && <th className="w-40 px-2 py-1 text-left font-normal">Self time</th>}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => (
                    <PlanLine key={r.id} r={r} analyzed={state.plan.analyzed} maxCost={maxCost} total={total} hot={hot?.id === r.id} picked={picked === r.id} onPick={() => setPicked(r.id === picked ? null : r.id)} />
                  ))}
                </tbody>
              </table>
            </div>
            {sel && (
              <aside className="w-80 shrink-0 overflow-auto border-l border-line p-3 text-[12px]" aria-label="Step details">
                <div className="font-medium">{sel.node.op}</div>
                {sel.node.object && <div className="font-mono text-[11.5px] text-muted">{sel.node.object}</div>}
                {sel.warnings.map((w) => (
                  <div key={w} className="mt-2 flex items-start gap-1.5 rounded bg-warning/12 px-2 py-1 text-warning">
                    <AlertTriangle size={12} className="mt-0.5 shrink-0" /> {w}
                  </div>
                ))}
                <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
                  {(
                    [
                      ["Estimated rows", sel.node.rows],
                      ["Actual rows", sel.node.actual_rows],
                      ["Loops", sel.node.loops],
                      ["Cost", sel.node.cost],
                    ] as const
                  )
                    .filter(([, v]) => v !== undefined)
                    .map(([k, v]) => (
                      <Fragment2 key={k} k={k} v={fmtNum(v!)} />
                    ))}
                  {sel.totalMs !== undefined && <Fragment2 k="Time (with children)" v={fmtMs(sel.totalMs)} />}
                  {sel.selfMs !== undefined && <Fragment2 k="Time (this step)" v={fmtMs(sel.selfMs)} />}
                  {sel.node.extra.map(([k, v]) => (
                    <Fragment2 key={k} k={k} v={v} mono />
                  ))}
                </dl>
              </aside>
            )}
          </div>
        </>
      )}
    </div>
  );
}

function Fragment2({ k, v, mono }: { k: string; v: string; mono?: boolean }) {
  return (
    <>
      <dt className="text-muted">{k}</dt>
      <dd className={`break-words ${mono ? "font-mono text-[11.5px]" : ""}`}>{v}</dd>
    </>
  );
}

function PlanLine({ r, analyzed, maxCost, total, hot, picked, onPick }: { r: PlanRow; analyzed: boolean; maxCost: number; total: number; hot: boolean; picked: boolean; onPick: () => void }) {
  const n = r.node;
  const share = total > 0 && r.selfMs !== undefined ? r.selfMs / total : 0;
  return (
    <tr
      role="treeitem"
      aria-level={r.depth + 1}
      aria-selected={picked}
      tabIndex={0}
      onClick={onPick}
      onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), onPick())}
      className={`cursor-pointer border-b border-line/50 ${picked ? "bg-accent/12" : "hover:bg-hover"}`}
    >
      <td className="px-2 py-1">
        <div className="flex min-w-0 items-center gap-1.5" style={{ paddingLeft: r.depth * 16 }}>
          {r.depth > 0 && <GitFork size={11} className="shrink-0 rotate-90 text-muted" />}
          <span className={`shrink-0 font-medium ${hot ? "text-warning" : ""}`}>{n.op}</span>
          {n.object && <span className="truncate font-mono text-[11.5px] text-muted">{n.object}</span>}
          {hot && <Flame size={12} className="shrink-0 text-warning" aria-label="slowest step" />}
          {r.warnings.length > 0 && (
            <span title={r.warnings.join("\n")} className="shrink-0 text-warning">
              <AlertTriangle size={12} aria-label={r.warnings.join("; ")} />
            </span>
          )}
        </div>
      </td>
      <td className="px-2 py-1 text-right tabular-nums text-muted">{n.rows !== undefined ? fmtNum(n.rows) : ""}</td>
      {analyzed && <td className="px-2 py-1 text-right tabular-nums">{n.actual_rows !== undefined ? fmtNum(n.actual_rows) : ""}</td>}
      {analyzed && <td className="px-2 py-1 text-right tabular-nums text-muted">{n.loops !== undefined && n.loops !== 1 ? fmtNum(n.loops) : ""}</td>}
      <td className="px-2 py-1 text-right tabular-nums text-muted">
        {n.cost !== undefined && (
          <span className="inline-flex items-center gap-1.5">
            <span className="inline-block h-1.5 rounded bg-accent/50" style={{ width: Math.max(2, (n.cost / (maxCost || 1)) * 48) }} />
            {fmtNum(n.cost)}
          </span>
        )}
      </td>
      {analyzed && (
        <td className="px-2 py-1">
          {r.selfMs !== undefined && (
            <span className="flex items-center gap-1.5 tabular-nums">
              <span className="h-2 w-20 overflow-hidden rounded bg-panel-2">
                <span className={`block h-full ${share > 0.3 ? "bg-warning" : "bg-accent/60"}`} style={{ width: `${Math.min(100, share * 100)}%` }} />
              </span>
              {fmtMs(r.selfMs)}
            </span>
          )}
        </td>
      )}
    </tr>
  );
}
