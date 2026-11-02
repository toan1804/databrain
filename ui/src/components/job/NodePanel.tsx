// Side panel of the selected step: name, action, connection, SQL or rows,
import { useEvent } from "../../lib/hooks";
// load or export settings, outcome and log.
import { useEffect, useMemo, useState } from "react";
import { AlertTriangle, FolderOpen, Info, Eye, Play, Square, Trash2 } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { Job, JobNode, JobRun, NodeRunSummary } from "../../lib/types";
import { nameProblem as nameFormatProblem, STEP_ACTIONS, actionSettings, changeAction, usesDefaultRows, readSql, upstreamOf } from "../../lib/jobGraph";
import { relativeTime } from "../../lib/util";
import { isQueryTab, useStore } from "../../store";
import { useJobs } from "../../jobsStore";
import { openOutput } from "../../outputs";
import { CellEditor } from "../Notebook";
import { ExportFields } from "./ExportFields";
import { LoadFields } from "./LoadFields";
import { RunLog } from "./RunLog";
import { EDITOR_KEY, NodeList, ProgressBar, StatusIcon, actionIcon, field, isResults, label, latestTab, statusText } from "./shared";

export function NodePanel({
  job,
  node,
  status,
  run,
  running,
  onChange,
  onRun,
  onSelect,
  onDelete,
  taken,
}: {
  job: Job;
  taken: Map<string, string>;
  node: JobNode;
  status: NodeRunSummary | null;
  /** The job's current or last run in this session. */
  run?: JobRun;
  running: boolean;
  onChange: (patch: Partial<JobNode>) => void;
  onRun: () => void;
  onSelect: (id: string) => void;
  onDelete: () => void;
}) {
  const connections = useStore((s) => s.connections);
  const allTabs = useStore((s) => s.tabs);
  const queryTabs = useMemo(() => allTabs.filter(isQueryTab), [allTabs]);
  const output = useStore((s) => s.outputs.find((x) => x.tab_id === EDITOR_KEY(job.id, node.id) && x.active && x.state !== "evicted"));
  const [name, setName] = useState(node.name);
  const rename = useEvent((name: string) => onChange({ name }));
  const ups = upstreamOf(job, node.id).map((id) => job.nodes.find((n) => n.id === id)!).filter(Boolean);
  const downs = job.edges.filter((e) => e.from === node.id).map((e) => job.nodes.find((n) => n.id === e.to)!).filter(Boolean);
  // Known at once: format, this job's steps, other jobs' steps and named outputs.
  const localProblem =
    nameFormatProblem(name) ??
    (job.nodes.some((n) => n.id !== node.id && n.name.toLowerCase() === name.toLowerCase())
      ? "Another step of this job has this name"
      : taken.has(name.toLowerCase())
        ? `results.${name} is already used by ${taken.get(name.toLowerCase())}`
        : null);
  // Then the app (notebook cells' output names too), shortly after typing.
  const [remote, setRemote] = useState<{ name: string; user: string | null } | null>(null);
  useEffect(() => {
    if (localProblem || name === node.name) return;
    let live = true;
    const t = setTimeout(() => {
      api
        .outputNameUser(name, job.id)
        .then((user) => {
          if (!live) return;
          setRemote({ name, user });
          if (!user) rename(name);
        })
        .catch(() => {});
    }, 250);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [name, localProblem, node.name, job.id, rename]);
  const nameProblem = localProblem ?? (remote?.name === name && remote.user ? `results.${name} is already used by ${remote.user}` : null);
  const results = connections.find(isResults);
  const runsOn = node.kind !== "query" || !node.connection_id ? results : connections.find((c) => c.id === node.connection_id);
  const others = connections.filter((c) => !isResults(c));
  const mentions = (u: JobNode) => new RegExp(`\\bresults\\s*\\.\\s*"?${u.name}"?\\b`, "i").test(node.sql);

  return (
    <div className="space-y-3 p-3">
      <div className="flex items-center gap-2">
        <div className="min-w-0 flex-1">
          <label className={label} htmlFor={`name-${node.id}`}>
            Step (output name)
          </label>
          <input
            id={`name-${node.id}`}
            className={`${field} font-mono ${nameProblem ? "border-danger" : ""}`}
            value={name}
            onChange={(e) => setName(e.target.value)}
            onBlur={() => nameProblem && setName(node.name)}
          />
          {nameProblem ? (
            <div className="mt-0.5 text-[11px] text-danger">{nameProblem}. Kept: {node.name}</div>
          ) : (
            <div className="mt-0.5 text-[11px] text-muted" title="Unique across the app: only this step produces this output">
              Output: <span className="font-mono">results.{name || node.name}</span>
            </div>
          )}
        </div>
        {status?.status === "running" ? (
          <button className="btn-ghost mt-4 py-1 text-danger" onClick={() => void useJobs.getState().cancelStep(job.id, node.id)}>
            <Square size={12} /> Stop this step
          </button>
        ) : (
          <button className="btn-primary mt-4 py-1" onClick={onRun} disabled={running} title="Run only this step (upstream outputs must exist)">
            <Play size={12} /> Run step
          </button>
        )}
        <button className="icon-btn mt-4 h-7 w-7" aria-label="Delete step" title="Delete step" onClick={onDelete}>
          <Trash2 size={13} />
        </button>
      </div>

      {node.ai_sql && (
        <div className="flex items-start gap-1.5 rounded-md border border-line px-2 py-1.5 text-[11.5px] text-muted">
          <Info size={12} className="mt-0.5 shrink-0" />
          <span className="min-w-0 flex-1">
            The AI wrote this step's SQL, so on DuckDB it runs in the AI's sandbox (no files, network or extensions) whoever runs the job. Changing the SQL makes it yours.
          </span>
          <button
            className="btn-ghost shrink-0 py-0.5 text-[11.5px]"
            title="Run this SQL as yours from now on: it can read files and the network on DuckDB"
            onClick={() => onChange({ ai_sql: false })}
          >
            Trust this SQL
          </button>
        </div>
      )}

      {/* Outcome first: what happened is what you look for after a run. */}
      <div className={`rounded-md border px-2 py-1.5 ${status?.status === "error" ? "border-danger/50 bg-danger/5" : status?.status === "success" ? "border-success/40" : "border-line"}`}>
        <div className="flex items-center gap-1.5">
          <StatusIcon status={status?.status} />
          <span className={`min-w-0 flex-1 whitespace-pre-wrap break-words ${status?.status === "error" ? "text-danger" : "text-muted"}`}>{statusText(status)}</span>
          {status?.status === "success" && status.file && (
            <button
              className="btn-ghost shrink-0 py-0.5 text-[11.5px]"
              title={status.file}
              onClick={() => api.revealJobFile(job.id, node.id).catch((e) => useStore.getState().toast(toError(e).message, "error"))}
            >
              <FolderOpen size={12} /> Show file
            </button>
          )}
          {output && (
            <button className="btn-ghost shrink-0 py-0.5 text-[11.5px]" onClick={() => void openOutput(output)}>
              <Eye size={12} /> View output
            </button>
          )}
        </div>
        {status?.status === "running" && status.progress && (
          <div className="mt-1.5">
            <ProgressBar p={status.progress} />
          </div>
        )}
        {status?.notices?.map((n, i) => (
          <div key={i} className="mt-1 flex items-start gap-1 text-[11.5px] text-accent">
            <Info size={12} className="mt-0.5 shrink-0" /> <span className="min-w-0 break-words">{n}</span>
          </div>
        ))}
        {run?.nodes.some((r) => r.node_id === node.id) && (
          <div className="mt-1.5 border-t border-line pt-1.5">
            <div className="mb-1 text-[11px] text-muted">Log ({run.status === "running" ? "running now" : relativeTime(run.started_at)})</div>
            <RunLog job={job} run={run} step={node.id} />
          </div>
        )}
      </div>

      <ActionPicker node={node} upstreams={ups.map((u) => u.name)} onChange={onChange} />

      {node.kind === "query" && (
        <div className="grid grid-cols-2 gap-2">
          <div>
            <label className={label} htmlFor={`conn-${node.id}`}>
              Connection
            </label>
            <select id={`conn-${node.id}`} className={field} value={runsOn && runsOn !== results ? runsOn.id : ""} onChange={(e) => onChange({ connection_id: e.target.value || null })}>
              <option value="">Results (DuckDB) · reads other steps</option>
              {others.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </select>
          </div>
          <div>
            <label className={label} htmlFor={`from-${node.id}`}>
              Copy from a query tab
            </label>
            <select
              id={`from-${node.id}`}
              className={field}
              value=""
              disabled={!queryTabs.length}
              onChange={(e) => {
                const found = queryTabs.find((x) => x.id === e.target.value);
                if (!found) return;
                const t = latestTab(found);
                const apply = () => onChange({ sql: t.sql, connection_id: t.connection_id ?? null });
                if (node.sql.trim() && node.sql !== t.sql)
                  useStore.getState().askConfirm({ title: `Replace the SQL of "${node.name}"?`, reasons: [`It gets the SQL and connection of "${t.title}".`], confirmLabel: "Replace", onConfirm: apply });
                else apply();
              }}
            >
              <option value="">{queryTabs.length ? "Choose a tab…" : "No query tabs open"}</option>
              {queryTabs.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.title}
                </option>
              ))}
            </select>
          </div>
        </div>
      )}

      {node.kind === "query" ? (
        <>
        {ups.length > 0 && (
          <div className="text-[11.5px]">
            <span className="text-muted" title={runsOn === results ? undefined : "Outputs of other steps can only be read by DuckDB steps; this step only waits for them."}>
              {runsOn === results ? "Reads (click to insert): " : "Runs after: "}
            </span>
            {ups.map((u, i) => (
              <span key={u.id}>
                {i > 0 && ", "}
                <button
                  className={`font-mono hover:underline ${runsOn === results && !mentions(u) ? "text-warning" : "text-accent"}`}
                  title={runsOn === results ? (mentions(u) ? `Uses results.${u.name}` : `Doesn't use results.${u.name} yet: click to add it`) : `Select ${u.name}`}
                  onClick={() => (runsOn === results && !mentions(u) ? onChange({ sql: node.sql.trim() ? `${node.sql.trimEnd()}\n-- results.${u.name}` : readSql([u.name]) }) : onSelect(u.id))}
                >
                  results.{u.name}
                </button>
              </span>
            ))}
          </div>
        )}

        <div>
          <label className={label}>
            SQL {runsOn ? `on ${runsOn === results ? "Results (DuckDB)" : runsOn.name}` : ""}
          </label>
          <CellEditor
            editorKey={EDITOR_KEY(job.id, node.id)}
            source={node.sql}
            connectionId={node.kind !== "query" ? (results?.id ?? null) : (node.connection_id ?? results?.id ?? null)}
            height={240}
            onResize={() => {}}
            onChange={(sql) => onChange({ sql })}
            onRun={onRun}
            onRunAdvance={onRun}
            onFocus={() => {}}
          />
        </div>
        </>
      ) : (
        <RowsSection job={job} node={node} ups={ups} onChange={onChange} onSelect={onSelect} />
      )}

      {node.kind === "export" && <ExportFields job={job} node={node} onChange={onChange} />}

      {node.kind === "load" && <LoadFields job={job} node={node} onChange={onChange} />}

      {(ups.length > 0 || downs.length > 0) && (
        <div className="grid grid-cols-2 gap-2 text-[11.5px]">
          <NodeList title="Upstream" nodes={ups} onSelect={onSelect} empty="none: runs first" />
          <NodeList title="Downstream" nodes={downs} onSelect={onSelect} empty="none" />
        </div>
      )}
    </div>
  );
}

/** The one thing this step does, and a way to change it (clearing the old action's settings). */
function ActionPicker({ node, upstreams, onChange }: { node: JobNode; upstreams: string[]; onChange: (patch: Partial<JobNode>) => void }) {
  const cur = STEP_ACTIONS.find((a) => a.kind === node.kind) ?? STEP_ACTIONS[0];
  const pick = (kind: JobNode["kind"]) => {
    const next = STEP_ACTIONS.find((a) => a.kind === kind)!;
    const apply = () => onChange(changeAction(node, kind, upstreams));
    const lost = actionSettings(node);
    if (!lost.length) return apply();
    useStore.getState().askConfirm({
      title: `Change "${node.name}" to "${next.title}"?`,
      reasons: [`A step does one thing. ${lost.join(", ").replace(/^./, (c) => c.toUpperCase())} will be removed.`, "To keep this action too, add another step after this one instead."],
      confirmLabel: "Change action",
      onConfirm: apply,
    });
  };
  return (
    <section aria-label="Action" className="flex items-center gap-2 rounded-md border border-accent/40 bg-accent/5 px-2 py-1.5" title={cur.hint}>
      {actionIcon(node.kind)}
      <div className="min-w-0 flex-1 truncate text-[12.5px] font-semibold">{cur.title}</div>
      <select aria-label="Change action" className="w-auto shrink-0 rounded-md border border-line bg-bg px-1.5 py-0.5 text-[11.5px]" value="" onChange={(e) => e.target.value && pick(e.target.value as JobNode["kind"])}>
        <option value="">Change…</option>
        {STEP_ACTIONS.filter((a) => a.kind !== node.kind).map((a) => (
          <option key={a.kind} value={a.kind}>
            {a.title}
          </option>
        ))}
      </select>
    </section>
  );
}

/** Rows a load/export step writes: all rows of its upstream step, or a DuckDB query. */
function RowsSection({ job, node, ups, onChange, onSelect }: { job: Job; node: JobNode; ups: JobNode[]; onChange: (patch: Partial<JobNode>) => void; onSelect: (id: string) => void }) {
  const names = ups.map((u) => u.name);
  const results = useStore((s) => s.connections.find(isResults));
  const plain = usesDefaultRows(node, names);
  const [editing, setEditing] = useState(!plain);
  const what = node.kind === "load" ? "load" : "save";
  return (
    <section aria-label="Rows" className="space-y-1.5">
      <div className={label}>Rows to {what}</div>
      {!ups.length && !node.sql.trim() && (
        <div className="flex items-start gap-1 text-[11.5px] text-warning">
          <AlertTriangle size={12} className="mt-0.5 shrink-0" /> Link an upstream step (drag its ● handle onto this one), or write SQL that selects the rows.
        </div>
      )}
      {!editing && ups.length > 0 ? (
        <div className="flex items-center gap-2 rounded-md border border-line px-2 py-1.5 text-[12px]">
          <span className="min-w-0 flex-1">
            All rows of{" "}
            <button className="font-mono text-accent hover:underline" onClick={() => onSelect(ups[0].id)}>
              results.{ups[0].name}
            </button>
            {ups.length > 1 && <span className="text-muted"> (the first upstream step; use SQL to combine several)</span>}
          </span>
          <button className="btn-ghost shrink-0 py-0.5 text-[11.5px]" onClick={() => (setEditing(true), onChange({ sql: node.sql.trim() ? node.sql : readSql(names) }))}>
            Filter or reshape with SQL…
          </button>
        </div>
      ) : (
        <div>
          <div className="mb-1 flex items-center gap-2 text-[11px] text-muted">
            <span className="flex-1">DuckDB query; upstream outputs are results.&lt;step&gt;</span>
            {ups.length > 0 && (
              <button className="btn-ghost py-0 text-[11px]" onClick={() => (setEditing(false), onChange({ sql: "" }))}>
                Use all rows of results.{ups[0].name}
              </button>
            )}
          </div>
          <CellEditor
            editorKey={EDITOR_KEY(job.id, node.id)}
            source={node.sql}
            connectionId={results?.id ?? null}
            height={160}
            onResize={() => {}}
            onChange={(sql) => onChange({ sql })}
            onRun={() => {}}
            onRunAdvance={() => {}}
            onFocus={() => {}}
          />
        </div>
      )}
    </section>
  );
}
