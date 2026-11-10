// Plan comparison: the plans of a statement before and after a rewrite
// (the AI's "Improve with AI" / optimize edits), side by side with what
// changed: cost, time (Analyze), steps, full scans, and on BigQuery,
// Snowflake and Databricks the bytes each would scan (cost preview).
// Both run on the source tab's session, one after the other.

import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, ArrowRight, ChevronDown, ChevronRight, GitFork, Loader2, Minus, Plus, RefreshCw } from "lucide-react";
import { api, toError } from "../lib/api";
import { fmtBytes } from "../lib/cost";
import { useEvent } from "../lib/hooks";
import { comparePlans, delta, flattenPlan, fmtDelta, fmtMs, fmtNum, type Delta, type PlanRow } from "../lib/plan";
import { traitsOf } from "../lib/sourceTraits";
import type { CostEstimate, QueryPlan } from "../lib/types";
import { lineDiff } from "../lib/util";
import { useStore } from "../store";
import { PLAN_KINDS } from "./PlanTab";
import { ConnDot } from "./ui";

/** Can this connection compare plans (a plan, or at least a cost estimate)? */
export function canComparePlans(kind: string | undefined): boolean {
  return !!kind && (!!PLAN_KINDS[kind] || traitsOf(kind as never).cost_preview);
}

/**
 * Open (or refresh) the before/after comparison of a rewrite of a
 * statement in query tab `sourceTabId`.
 */
export async function openPlanCompare(sourceTabId: string, before: string, after: string) {
  const st = useStore.getState();
  const src = st.tabs.find((t) => t.id === sourceTabId);
  const conn = st.connections.find((c) => c.id === src?.connection_id);
  if (!src || !conn) return st.toast("The tab of this edit is closed or has no connection", "error");
  if (!canComparePlans(conn.config.kind)) return st.toast(`${traitsOf(conn.config.kind).name} has no query plan or cost estimate DataBrain can read`, "info");
  if (!before.trim() || !after.trim()) return st.toast("Both the original and the rewritten statement are needed", "info");
  const params = await st.paramsFor(sourceTabId, conn.config.kind, `${before}\n${after}`);
  if (!params) return;
  const plan = { source: sourceTabId, analyze: false, params, compare: { before, after } };
  const existing = st.tabs.find((t) => t.plan?.compare && t.plan.source === sourceTabId);
  if (existing) {
    st.updateTab(existing.id, { sql: after, plan, connection_id: conn.id });
    return st.setActiveTab(existing.id);
  }
  st.newTab({ title: `Plans · ${src.title}`, sql: after, connection_id: conn.id, plan });
}

interface Side {
  plan?: QueryPlan;
  planError?: string;
  cost?: CostEstimate;
  costError?: string;
}

type State = { status: "loading" } | { status: "ready"; before: Side; after: Side };

export function PlanCompareTab({ tabId, visible }: { tabId: string; visible: boolean }) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const conn = useStore((s) => s.connections.find((c) => c.id === tab?.connection_id));
  const updateTab = useStore((s) => s.updateTab);
  const ref = tab?.plan;
  const pair = ref?.compare;
  const [state, setState] = useState<State>({ status: "loading" });
  const [view, setView] = useState<"tree" | "raw">("tree");
  const [sqlOpen, setSqlOpen] = useState(false);
  const seq = useRef(0);
  const kind = conn?.config.kind;
  const hasPlan = !!kind && !!PLAN_KINDS[kind];
  const canAnalyze = !!kind && !!PLAN_KINDS[kind]?.analyze;
  const hasCost = !!kind && traitsOf(kind).cost_preview;

  const load = useEvent(async () => {
    if (!ref || !pair || !conn) return;
    const my = ++seq.current;
    setState({ status: "loading" });
    // One after the other: both use the source tab's session.
    const side = async (sql: string): Promise<Side> => {
      const s: Side = {};
      if (hasPlan) {
        try {
          s.plan = await api.explainPlan(conn.id, ref.source, sql, ref.analyze, ref.params);
        } catch (e) {
          s.planError = toError(e).message;
        }
      }
      if (hasCost) {
        try {
          s.cost = await api.estimateCost(conn.id, ref.source, sql, ref.params);
        } catch (e) {
          s.costError = toError(e).message;
        }
      }
      return s;
    };
    const before = await side(pair.before);
    const after = await side(pair.after);
    if (my === seq.current) setState({ status: "ready", before, after });
  });

  useEffect(() => {
    void load();
  }, [pair?.before, pair?.after, ref?.analyze, conn?.id, load]);

  const cmp = useMemo(() => {
    if (state.status !== "ready" || !state.before.plan || !state.after.plan) return null;
    return comparePlans(state.before.plan, state.after.plan);
  }, [state]);
  const diff = useMemo(() => (pair ? lineDiff(pair.before, pair.after) : []), [pair]);

  if (!tab || !ref || !pair) return null;
  const setAnalyze = (analyze: boolean) => updateTab(tab.id, { plan: { ...ref, analyze } });
  const ready = state.status === "ready" ? state : null;
  const noTree = !!ready && [ready.before.plan, ready.after.plan].some((p) => p && p.roots.length === 0);
  const bytes = ready ? delta(ready.before.cost?.bytes ?? undefined, ready.after.cost?.bytes ?? undefined) : null;

  return (
    <div className="flex h-full min-h-0 flex-col" style={{ display: visible ? "flex" : "none" }}>
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-line px-2 text-[12px]">
        <ConnDot color={conn?.color} connected={conn?.connected} />
        <span className="shrink-0 text-muted">{conn?.name ?? "No connection"}</span>
        <span className="flex min-w-0 flex-1 items-center gap-1.5 font-medium">
          <GitFork size={13} className="rotate-90 text-muted" /> Before <ArrowRight size={12} className="text-muted" /> after the rewrite
        </span>
        {hasPlan && (
          <div className="flex shrink-0 rounded-md border border-line p-0.5" role="radiogroup" aria-label="Plan kind">
            {([false, true] as const).map((a) => (
              <button
                key={String(a)}
                role="radio"
                aria-checked={ref.analyze === a}
                disabled={a && !canAnalyze}
                title={a ? (canAnalyze ? "Run both statements and measure them (EXPLAIN ANALYZE; read queries only)" : "This database's plans can't be analyzed here") : "The planner's estimates; nothing runs"}
                onClick={() => setAnalyze(a)}
                className={`h-5 rounded px-2 text-[11.5px] disabled:opacity-40 ${ref.analyze === a ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
              >
                {a ? "Analyze" : "Estimate"}
              </button>
            ))}
          </div>
        )}
        {hasPlan && (
          <div className="flex shrink-0 rounded-md border border-line p-0.5" role="radiogroup" aria-label="View">
            {(["tree", "raw"] as const).map((v) => (
              <button key={v} role="radio" aria-checked={view === v} onClick={() => setView(v)} className={`h-5 rounded px-2 text-[11.5px] ${view === v ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}>
                {v === "tree" ? "Tree" : "Raw"}
              </button>
            ))}
          </div>
        )}
        <button className="icon-btn" title="Explain both again" aria-label="Refresh plans" onClick={() => void load()}>
          <RefreshCw size={14} />
        </button>
      </div>

      {!ready ? (
        <div className="flex flex-1 items-center justify-center gap-2 text-muted">
          <Loader2 size={16} className="animate-spin" /> {ref.analyze ? "Running both statements to measure them…" : "Asking the planner about both statements…"}
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-auto">
          <table className="m-3 border-collapse text-[12px]" aria-label="What changed">
            <thead className="text-[11px] text-muted">
              <tr>
                <th className="px-2 py-1 text-left font-normal" />
                <th className="px-2 py-1 text-right font-normal">Before</th>
                <th className="px-2 py-1 text-right font-normal">After</th>
                <th className="px-2 py-1 text-left font-normal">Change</th>
              </tr>
            </thead>
            <tbody>
              {cmp && <MetricRow label="Estimated cost" d={cmp.cost} fmt={fmtNum} />}
              {cmp && ref.analyze && <MetricRow label="Time" d={cmp.time} fmt={fmtMs} />}
              {cmp && <MetricRow label="Estimated rows" d={delta(cmp.before.rows, cmp.after.rows, false)} fmt={fmtNum} neutral />}
              {cmp && cmp.before.steps + cmp.after.steps > 0 && <MetricRow label="Steps" d={cmp.steps} fmt={String} neutral />}
              {cmp && <MetricRow label="Full scans" d={cmp.scans} fmt={String} />}
              {bytes && bytes.before !== undefined && <MetricRow label="Bytes scanned" d={bytes} fmt={fmtBytes} />}
              {ready.before.cost?.partitions_total != null && (
                <MetricRow label="Partitions scanned" d={delta(ready.before.cost.partitions_assigned ?? undefined, ready.after.cost?.partitions_assigned ?? undefined)} fmt={fmtNum} />
              )}
            </tbody>
          </table>
          {!cmp && !bytes?.before && <p className="mx-3 text-[12px] text-muted">Nothing to compare yet: see the messages below.</p>}
          {cmp && (cmp.scansRemoved.length > 0 || cmp.scansAdded.length > 0) && (
            <ul className="mx-3 mb-2 space-y-0.5 text-[12px]">
              {cmp.scansRemoved.map((s, i) => (
                <li key={`r${i}`} className="text-success">
                  No longer: {s}
                </li>
              ))}
              {cmp.scansAdded.map((s, i) => (
                <li key={`a${i}`} className="text-warning">
                  New full scan: {s}
                </li>
              ))}
            </ul>
          )}
          {[
            ["Before", ready.before],
            ["After", ready.after],
          ].flatMap(([label, s]) => {
            const side = s as Side;
            return [side.planError && `${label}: ${side.planError}`, side.costError && `${label} (cost): ${side.costError}`].filter(Boolean) as string[];
          }).map((m) => (
            <p key={m} className="mx-3 mb-1 flex items-start gap-1.5 text-[12px] text-danger" role="alert">
              <AlertTriangle size={12} className="mt-0.5 shrink-0" /> {m}
            </p>
          ))}
          {cmp && !ref.analyze && <p className="mx-3 mb-2 text-[11.5px] text-muted">Costs are the planner's estimates in {traitsOf(kind!).name}'s own units: compare them with each other, not with other databases.{canAnalyze ? " Analyze measures both." : ""}</p>}

          <button className="mx-3 mb-1 flex items-center gap-1 text-[12px] text-muted hover:text-fg" onClick={() => setSqlOpen(!sqlOpen)} aria-expanded={sqlOpen}>
            {sqlOpen ? <ChevronDown size={12} /> : <ChevronRight size={12} />} SQL changes
          </button>
          {sqlOpen && (
            <div className="mx-3 mb-3 max-h-64 overflow-auto rounded-md border border-line font-mono text-[11.5px] leading-snug select-text">
              {diff.map((d, i) => (
                <div key={i} className={`whitespace-pre-wrap px-2 ${d.op === "add" ? "bg-success/12" : d.op === "del" ? "bg-danger/12 text-muted line-through" : ""}`}>
                  <span className="mr-1.5 inline-block w-2 select-none text-muted">{d.op === "add" ? "+" : d.op === "del" ? "−" : " "}</span>
                  {d.text || " "}
                </div>
              ))}
            </div>
          )}

          {hasPlan && (
            <div className="grid grid-cols-2 gap-2 px-3 pb-3">
              {(["before", "after"] as const).map((k) => {
                const p = ready[k].plan;
                return (
                  <section key={k} className="min-w-0 rounded-md border border-line" aria-label={`Plan ${k}`}>
                    <h3 className="border-b border-line bg-panel-2 px-2 py-1 text-[11.5px] font-medium">{k === "before" ? "Before" : "After"}</h3>
                    {!p ? (
                      <p className="p-2 text-[12px] text-muted">No plan.</p>
                    ) : view === "raw" || noTree ? (
                      <pre className="max-h-[60vh] overflow-auto p-2 font-mono text-[11px]">{p.raw || "The database returned no plan."}</pre>
                    ) : (
                      <PlanSide rows={flattenPlan(p.roots)} analyzed={p.analyzed} marked={k === "before" ? cmp?.gone : cmp?.added} mark={k === "before" ? "gone" : "added"} />
                    )}
                  </section>
                );
              })}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function MetricRow({ label, d, fmt, neutral }: { label: string; d: Delta; fmt: (n: number) => string; neutral?: boolean }) {
  const tone = neutral || d.better === null ? "text-muted" : d.better ? "text-success" : "text-warning";
  return (
    <tr className="border-t border-line/50">
      <td className="px-2 py-1 text-muted">{label}</td>
      <td className="px-2 py-1 text-right tabular-nums">{d.before !== undefined ? fmt(d.before) : "–"}</td>
      <td className="px-2 py-1 text-right tabular-nums">{d.after !== undefined ? fmt(d.after) : "–"}</td>
      <td className={`px-2 py-1 tabular-nums ${tone}`}>{fmtDelta(d)}</td>
    </tr>
  );
}

function PlanSide({ rows, analyzed, marked, mark }: { rows: PlanRow[]; analyzed: boolean; marked?: Set<string>; mark: "gone" | "added" }) {
  return (
    <div className="max-h-[60vh] overflow-auto" role="tree">
      <table className="w-full border-collapse text-[11.5px]">
        <thead className="sticky top-0 bg-panel text-[10.5px] text-muted">
          <tr>
            <th className="px-2 py-0.5 text-left font-normal">Step</th>
            <th className="px-2 py-0.5 text-right font-normal">Rows</th>
            <th className="px-2 py-0.5 text-right font-normal">{analyzed ? "Time" : "Cost"}</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => {
            const m = marked?.has(r.id);
            return (
              <tr
                key={r.id}
                role="treeitem"
                aria-level={r.depth + 1}
                className={`border-t border-line/40 ${m ? (mark === "added" ? "bg-success/8" : "bg-danger/8") : ""}`}
                title={m ? (mark === "added" ? "Only in the rewrite" : "Not in the rewrite") : undefined}
              >
                <td className="px-2 py-0.5">
                  <div className="flex min-w-0 items-center gap-1" style={{ paddingLeft: r.depth * 12 }}>
                    {m && (mark === "added" ? <Plus size={10} className="shrink-0 text-success" /> : <Minus size={10} className="shrink-0 text-danger" />)}
                    <span className="shrink-0 font-medium">{r.node.op}</span>
                    {r.node.object && <span className="truncate font-mono text-[10.5px] text-muted">{r.node.object}</span>}
                    {r.warnings.length > 0 && (
                      <span title={r.warnings.join("\n")} className="shrink-0 text-warning">
                        <AlertTriangle size={10} aria-label={r.warnings.join("; ")} />
                      </span>
                    )}
                  </div>
                </td>
                <td className="px-2 py-0.5 text-right tabular-nums text-muted">{(r.node.actual_rows ?? r.node.rows) !== undefined ? fmtNum((r.node.actual_rows ?? r.node.rows)!) : ""}</td>
                <td className="px-2 py-0.5 text-right tabular-nums text-muted">
                  {analyzed ? (r.selfMs !== undefined ? fmtMs(r.selfMs) : "") : r.node.cost !== undefined ? fmtNum(r.node.cost) : ""}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
