// Side panel with no step selected: the job's schedule and its runs.
import { useEffect, useState } from "react";
import { Play, RotateCcw, Variable } from "lucide-react";
import { api } from "../../lib/api";
import type { ConnectorKind, Job, JobRun, ParamValue, RunParams } from "../../lib/types";
import { useStore } from "../../store";
import { formatDuration, relativeTime } from "../../lib/util";
import { useJobs } from "../../jobsStore";
import { RunLog } from "./RunLog";
import { JobLimits } from "./LimitFields";
import { ScheduleSection } from "./ScheduleSection";
import { triggerText } from "../../lib/jobGraph";
import { ProgressBar, StatusIcon, field, label, statusText, type Patch } from "./shared";

/** "2 succeeded · 1 failed" for a run's steps. */
function runCounts(r: JobRun): string {
  const n = (st: string) => r.nodes.filter((x) => x.status === st).length;
  return [
    [n("success"), "succeeded"],
    [n("error"), "failed"],
    [n("skipped"), "skipped"],
    [n("cancelled"), "stopped"],
    [n("running"), "running"],
    [n("waiting"), "waiting"],
  ]
    .filter(([c]) => c)
    .map(([c, w]) => `${c} ${w}`)
    .join(" · ");
}

/** `:name` parameters the job's SQL uses (step SQL, load before/after SQL), in order. */
function useJobParamNames(job: Job): string[] {
  const connections = useStore((s) => s.connections);
  const [names, setNames] = useState<string[]>([]);
  useEffect(() => {
    let alive = true;
    const kindOf = (id: string | null | undefined): ConnectorKind => (connections.find((c) => c.id === id)?.config.kind ?? "duckdb") as ConnectorKind;
    const texts: [ConnectorKind, string][] = [];
    for (const n of job.nodes) {
      texts.push([n.kind === "query" ? kindOf(n.connection_id) : "duckdb", n.sql]);
      for (const extra of [n.load_before_sql, n.load_after_sql]) if (extra) texts.push([kindOf(n.target_connection_id), extra]);
    }
    const t = setTimeout(() => {
      Promise.all(texts.filter(([, sql]) => sql.includes(":") || sql.includes("&")).map(([k, sql]) => api.sqlParameters(k, sql).catch(() => [])))
        .then((all) => alive && setNames([...new Set(all.flat().map((p) => p.name))]))
        .catch(() => {});
    }, 300);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [job.nodes, connections]);
  return names;
}

/** The job's `:name` values: a saved set, values of its own over it, and "Run with" another set. */
function JobParams({ job, onChange, onRunWith }: { job: Job; onChange: (patch: Partial<Job>) => void; onRunWith: (p: RunParams) => void }) {
  const names = useJobParamNames(job);
  const sets = useStore((s) => s.paramSets);
  useEffect(() => {
    void useStore.getState().loadParamSets();
  }, []);
  const own = job.params ?? {};
  const set = sets.find((s) => s.id === job.param_set);
  const missingSet = !!job.param_set && !set && sets.length > 0;
  const setValue = (n: string, value: string) => {
    const next: Record<string, ParamValue> = Object.fromEntries(Object.entries(own).filter((e): e is [string, ParamValue] => !!e[1]));
    if (value.trim()) next[n] = { ...next[n], value };
    else delete next[n];
    onChange({ params: next });
  };
  if (!names.length && !Object.keys(own).length && !job.param_set) return null;
  return (
    <section className="space-y-2">
      <div className={`${label} flex items-center gap-1`}>
        <Variable size={11} /> Parameters
      </div>
      <div className="flex items-center gap-2">
        <select
          className={`${field} min-w-0 flex-1`}
          aria-label="Parameter set of the job"
          value={job.param_set ?? ""}
          onChange={(e) => onChange({ param_set: e.target.value || undefined })}
        >
          <option value="">No parameter set</option>
          {sets.map((s) => (
            <option key={s.id} value={s.id}>
              {s.name}
            </option>
          ))}
        </select>
        {sets.length > 0 && (
          <select
            className={`${field} w-auto`}
            aria-label="Run with another parameter set"
            value=""
            onChange={(e) => e.target.value && onRunWith({ set: e.target.value })}
            title="Run now with another set's values (the job keeps its own)"
          >
            <option value="">Run with…</option>
            {sets.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}
              </option>
            ))}
          </select>
        )}
      </div>
      {missingSet && <div className="text-[11.5px] text-danger">The job's parameter set was deleted: choose another, or runs fail.</div>}
      {names.map((n) => {
        const fromSet = set?.values[n]?.value;
        const mine = own[n]?.value ?? "";
        return (
          <div key={n} className="flex items-center gap-2">
            <label className="w-28 shrink-0 truncate font-mono text-[12px] text-accent" htmlFor={`jp-${n}`}>
              :{n}
            </label>
            <input
              id={`jp-${n}`}
              className={`${field} font-mono ${!mine && !fromSet ? "border-dashed border-warning" : ""}`}
              placeholder={fromSet ? `${fromSet} (from ${set!.name})` : "needs a value"}
              value={mine}
              onChange={(e) => setValue(n, e.target.value)}
            />
          </div>
        );
      })}
      <div className="flex items-start gap-1 text-[11.5px] text-muted">
        <Play size={11} className="mt-0.5 shrink-0" />
        Every run fills the steps' :name with these values (a value here wins over the set's), on the schedule too. From a terminal: databrain-jobs run "{job.name}" --set NAME --param name=value.
      </div>
    </section>
  );
}

export function JobPanel({
  job,
  run,
  canResume,
  onResume,
  onRunWith,
  onChange,
  onSelect,
}: {
  job: Job;
  run?: JobRun;
  /** The newest run can be resumed (it failed or was stopped). */
  canResume: boolean;
  onResume: () => void;
  /** Run now with other parameter values. */
  onRunWith: (params: RunParams) => void;
  onChange: (patch: Patch<Job>) => void;
  onSelect: (id: string) => void;
}) {
  const summary = useJobs((s) => s.jobs.find((j) => j.id === job.id));
  const [runs, setRuns] = useState<JobRun[]>([]);
  // The run shown in full; the newest one unless another was picked.
  const [picked, setPicked] = useState<number | null>(null);
  useEffect(() => {
    if (run?.status === "running") return;
    api
      .jobRuns(job.id, 30)
      .then(setRuns)
      .catch(() => {});
  }, [job.id, run?.status, run?.id]);
  const shown = run && !runs.some((r) => r.id === run.id) ? [run, ...runs] : runs.map((r) => (run && r.id === run.id ? run : r));
  // A new run takes over the details.
  useEffect(() => setPicked(null), [run?.id]);
  const current = shown.find((r) => r.id === picked) ?? shown[0];

  return (
    <div className="space-y-4 p-3">
      <JobParams job={job} onChange={onChange} onRunWith={onRunWith} />
      <ScheduleSection job={job} nextRunAt={summary?.next_run_at ?? null} onChange={onChange} />

      <JobLimits job={job} onChange={onChange} />

      <section className="space-y-2">
        <div className={label}>Runs</div>
        {!current ? (
          <div className="text-muted">No runs yet. Use Run job above.</div>
        ) : (
          <RunDetails job={job} run={current} newest={current === shown[0]} canResume={canResume} onResume={onResume} onSelect={onSelect} />
        )}
        {shown.length > 1 && (
          <ul className="divide-y divide-line rounded-md border border-line" aria-label="Earlier runs">
            {shown.map((r) => (
              <li key={r.id}>
                <button
                  className={`flex w-full items-center gap-1.5 px-2 py-1 text-left hover:bg-panel-2 ${r === current ? "bg-accent/10" : ""}`}
                  aria-current={r === current}
                  onClick={() => setPicked(r.id)}
                >
                  <StatusIcon status={r.status} />
                  <span className="shrink-0" title={new Date(r.started_at).toLocaleString()}>
                    {relativeTime(r.started_at)}
                  </span>
                  <span className="min-w-0 flex-1 truncate text-[11px] text-muted">
                    {triggerText(r.trigger)} · {runCounts(r) || "no steps"}
                  </span>
                  <span className="shrink-0 text-[11px] text-muted">{r.finished_at ? formatDuration(r.finished_at - r.started_at) : "running"}</span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}

/** One run in full: outcome, its steps and its log. */
function RunDetails({ job, run: r, newest, canResume, onResume, onSelect }: { job: Job; run: JobRun; newest: boolean; canResume: boolean; onResume: () => void; onSelect: (id: string) => void }) {
  const tone = r.status === "error" ? "border-danger/50 bg-danger/5" : r.status === "success" ? "border-success/40" : r.status === "running" ? "border-accent/50" : "border-line";
  const title = r.status === "running" ? "Running" : r.status === "success" ? "Succeeded" : r.status === "error" ? "Failed" : "Stopped";
  return (
    <div className={`space-y-2 rounded-md border p-2 ${tone}`}>
      <div className="flex items-center gap-1.5">
        <StatusIcon status={r.status} />
        <span className="font-medium">{title}</span>
        <span className="min-w-0 flex-1 truncate text-[11px] text-muted" title={new Date(r.started_at).toLocaleString()}>
          {relativeTime(r.started_at)} · {triggerText(r.trigger)}
          {r.finished_at ? ` · ${formatDuration(r.finished_at - r.started_at)}` : ""}
        </span>
        {newest && canResume && (
          <button className="btn-primary shrink-0 py-0.5 text-[11.5px]" onClick={onResume} title="Run again the steps that did not succeed, and the steps after them">
            <RotateCcw size={12} /> Resume
          </button>
        )}
      </div>
      {r.status === "error" && r.error && <div className="whitespace-pre-wrap break-words text-[11.5px] text-danger">{r.error}</div>}
      {r.nodes.length > 0 && (
        <ul className="space-y-1">
          {r.nodes.map((n) => (
            <li key={n.node_id}>
              <div className="flex items-start gap-1.5">
                <span className="mt-0.5">
                  <StatusIcon status={n.status} />
                </span>
                <button className="shrink-0 font-mono hover:underline" onClick={() => onSelect(n.node_id)} title="Select this step">
                  {n.name}
                </button>
                <span className={`min-w-0 flex-1 truncate text-[11px] ${n.status === "error" ? "text-danger" : "text-muted"}`} title={statusText(n)}>
                  {statusText(n)}
                </span>
              </div>
              {n.status === "running" && n.progress && (
                <div className="pl-5">
                  <ProgressBar p={n.progress} />
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
      <RunLog job={job} run={r} onSelect={onSelect} />
    </div>
  );
}
