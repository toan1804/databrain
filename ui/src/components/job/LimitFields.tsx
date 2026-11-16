// Time limits, retries and steps at once: the job's (JobLimits) and a
// step's own over them (StepLimitFields).
import { useEffect, useState } from "react";
import { Timer } from "lucide-react";
import type { Job, JobNode, RunLimits, StepLimits } from "../../lib/types";
import { field, label, useCommitOnUnmount, type Patch } from "./shared";

/** Most retries a step may have (the runner's `MAX_RETRIES`). */
export const MAX_RETRIES = 10;
/** Steps at once when the job doesn't say (the runner's defaults). */
export const DEFAULT_MAX_PARALLEL = 4;
export const DEFAULT_MAX_PER_CONNECTION = 2;
const MAX_SECONDS = 4_294_967_295;

/**
 * Seconds of a duration typed as `90s`, `30m`, `2h`, `1h30m` or a plain
 * number of minutes; `null` for an empty field, `"invalid"` otherwise (0,
 * negative, unknown units).
 */
export function parseDuration(text: string): number | null | "invalid" {
  const t = text.trim().toLowerCase().replace(/\s+/g, "");
  if (!t) return null;
  if (/^\d+(\.\d+)?$/.test(t)) {
    const s = Math.round(Number(t) * 60);
    return s >= 1 && s <= MAX_SECONDS ? s : "invalid";
  }
  const m = /^(?:(\d+)h)?(?:(\d+)(?:m|min))?(?:(\d+)s)?$/.exec(t);
  if (!m || (!m[1] && !m[2] && !m[3])) return "invalid";
  const s = Number(m[1] ?? 0) * 3600 + Number(m[2] ?? 0) * 60 + Number(m[3] ?? 0);
  return s >= 1 && s <= MAX_SECONDS ? s : "invalid";
}

/** `secs` as typed in a duration field: 1800 → "30m", 5400 → "1h30m", 90 → "1m30s". */
export function durationText(secs: number | null | undefined): string {
  if (!secs) return "";
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  return `${h ? `${h}h` : ""}${m ? `${m}m` : ""}${s ? `${s}s` : ""}`;
}

/** A whole number from `min` to `max`; `null` for an empty field. */
export function parseCount(text: string, min: number, max = MAX_SECONDS): number | null | "invalid" {
  const t = text.trim();
  if (!t) return null;
  if (!/^\d+$/.test(t)) return "invalid";
  const n = Number(t);
  return n >= min && n <= max ? n : "invalid";
}

/**
 * A field saved when left (or on Enter, or when the panel closes) if what
 * was typed is valid; an invalid value stays, to be corrected, with what's
 * wrong next to it, and isn't saved.
 */
function LimitInput({
  id,
  value,
  parse,
  show,
  onCommit,
  onLeave,
  placeholder,
  problem,
  className = "",
}: {
  id: string;
  value: number | null | undefined;
  parse: (text: string) => number | null | "invalid";
  show: (v: number | null | undefined) => string;
  /** `undefined`: emptied (the default). */
  onCommit: (v: number | undefined) => void;
  /** Saves what was typed when the panel closes (`base`: the value it was typed over). */
  onLeave: (v: number | undefined, base: number | null | undefined) => void;
  placeholder: string;
  problem: string;
  className?: string;
}) {
  const [text, setText] = useState(show(value));
  const [bad, setBad] = useState(false);
  useEffect(() => setText(show(value)), [value, show]);
  const commit = () => {
    const v = parse(text);
    if (v === "invalid") return setBad(true);
    setBad(false);
    if (v !== (value ?? null)) onCommit(v ?? undefined);
    setText(show(v));
  };
  useCommitOnUnmount(() => {
    const v = parse(text);
    if (v !== "invalid" && v !== (value ?? null)) onLeave(v ?? undefined, value);
  });
  const errId = `${id}-problem`;
  return (
    <div className={className}>
      <input
        id={id}
        className={`${field} ${bad ? "border-danger" : ""}`}
        value={text}
        placeholder={placeholder}
        aria-invalid={bad || undefined}
        aria-describedby={bad ? errId : undefined}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === "Enter" && commit()}
      />
      {bad && (
        <div id={errId} className="mt-0.5 text-[11px] text-danger">
          {problem}
        </div>
      )}
    </div>
  );
}

const countText = (v: number | null | undefined) => (v == null ? "" : String(v));
const DURATION_PROBLEM = "Type a duration such as 90s, 30m, 2h or 1h30m (a plain number is minutes)";

/** A step's time limit: a duration, or 0 to turn the job's off. */
export const durationOrOff = (t: string) => (/^(?:0+(?:h|m|min|s)?)+$/.test(t.trim().toLowerCase().replace(/\s+/g, "")) ? 0 : parseDuration(t));
const showOrOff = (v: number | null | undefined) => (v === 0 ? "0" : durationText(v));

/** Drop empty fields, so a job without limits saves none (left out, not null: the runner reads a missing field as none). */
export function compactLimits<T extends object>(o: T): T | undefined {
  const kept = Object.fromEntries(Object.entries(o).filter(([, v]) => v != null));
  return Object.keys(kept).length ? (kept as T) : undefined;
}

/** The job's time limits, retries and steps at once. */
export function JobLimits({ job, onChange }: { job: Job; onChange: (patch: Patch<Job>) => void }) {
  const l: RunLimits = job.limits ?? {};
  const set = (patch: Partial<RunLimits>) => onChange({ limits: compactLimits({ ...l, ...patch }) });
  // On close: onto the job as it is then, and not over a value changed meanwhile.
  const leave = (k: keyof RunLimits) => (v: number | undefined, base: number | null | undefined) =>
    onChange((j) => ((j.limits?.[k] ?? null) === (base ?? null) ? { limits: compactLimits({ ...(j.limits ?? {}), [k]: v }) } : null));
  const row = "grid grid-cols-[1fr_96px] items-start gap-2";
  const name = "pt-1 text-[12px]";
  return (
    <section className="space-y-1.5" aria-label="Limits">
      <div className={`${label} flex items-center gap-1`}>
        <Timer size={11} /> Limits
      </div>
      <div className={row}>
        <label className={name} htmlFor={`run-timeout-${job.id}`}>
          Run time limit
        </label>
        <LimitInput id={`run-timeout-${job.id}`} value={l.run_timeout_secs} parse={parseDuration} show={durationText} placeholder="none" problem={DURATION_PROBLEM} onCommit={(v) => set({ run_timeout_secs: v })} onLeave={leave("run_timeout_secs")} />
      </div>
      <div className={row}>
        <label className={name} htmlFor={`step-timeout-${job.id}`}>
          Step time limit
        </label>
        <LimitInput id={`step-timeout-${job.id}`} value={l.step_timeout_secs} parse={parseDuration} show={durationText} placeholder="none" problem={DURATION_PROBLEM} onCommit={(v) => set({ step_timeout_secs: v })} onLeave={leave("step_timeout_secs")} />
      </div>
      <div className={row}>
        <label className={name} htmlFor={`retries-${job.id}`} title="After a connection error, only for steps that can't write twice">
          Retries after a connection error
        </label>
        <LimitInput
          id={`retries-${job.id}`}
          value={l.retries}
          parse={(t) => parseCount(t, 0, MAX_RETRIES)}
          show={countText}
          placeholder="0"
          problem={`A whole number from 0 to ${MAX_RETRIES}`}
          onCommit={(v) => set({ retries: v })} onLeave={leave("retries")}
        />
      </div>
      <div className={row}>
        <label className={name} htmlFor={`parallel-${job.id}`}>
          Steps at once
        </label>
        <LimitInput
          id={`parallel-${job.id}`}
          value={l.max_parallel}
          parse={(t) => parseCount(t, 1, 64)}
          show={countText}
          placeholder={String(DEFAULT_MAX_PARALLEL)}
          problem="A whole number from 1 to 64"
          onCommit={(v) => set({ max_parallel: v })} onLeave={leave("max_parallel")}
        />
      </div>
      <div className={row}>
        <label className={name} htmlFor={`per-conn-${job.id}`}>
          Steps at once per connection
        </label>
        <LimitInput
          id={`per-conn-${job.id}`}
          value={l.max_per_connection}
          parse={(t) => parseCount(t, 1, 64)}
          show={countText}
          placeholder={String(DEFAULT_MAX_PER_CONNECTION)}
          problem="A whole number from 1 to 64"
          onCommit={(v) => set({ max_per_connection: v })} onLeave={leave("max_per_connection")}
        />
      </div>
      <div className="text-[11.5px] text-muted">
        A run or step past its time limit is stopped and fails. Retries wait 30 s, then 2 min, 8 min, 30 min; only steps that read, exports, and loads rolled back as a whole are tried again, never a write that may have been kept. Steps over a limit wait for a free slot.
      </div>
    </section>
  );
}

/** A step's own time limit and retries (empty: the job's; 0: none for this step). */
export function StepLimitFields({ job, node, onChange }: { job: Job; node: JobNode; onChange: (patch: Patch<JobNode>) => void }) {
  const l: StepLimits = node.limits ?? {};
  const jl: RunLimits = job.limits ?? {};
  const set = (patch: Partial<StepLimits>) => onChange({ limits: compactLimits({ ...l, ...patch }) });
  const leave = (k: keyof StepLimits) => (v: number | undefined, base: number | null | undefined) =>
    onChange((n) => ((n.limits?.[k] ?? null) === (base ?? null) ? { limits: compactLimits({ ...(n.limits ?? {}), [k]: v }) } : null));
  return (
    <section aria-label="Step limits" className="grid grid-cols-2 gap-2">
      <div>
        <label className={label} htmlFor={`timeout-${node.id}`}>
          Time limit
        </label>
        <LimitInput
          id={`timeout-${node.id}`}
          value={l.timeout_secs}
          parse={durationOrOff}
          show={showOrOff}
          placeholder={jl.step_timeout_secs ? `job's: ${durationText(jl.step_timeout_secs)}` : "none"}
          problem={`${DURATION_PROBLEM}; 0 for none`}
          onCommit={(v) => set({ timeout_secs: v })} onLeave={leave("timeout_secs")}
        />
      </div>
      <div>
        <label className={label} htmlFor={`retries-${node.id}`}>
          Retries
        </label>
        <LimitInput
          id={`retries-${node.id}`}
          value={l.retries}
          parse={(t) => parseCount(t, 0, MAX_RETRIES)}
          show={countText}
          placeholder={jl.retries ? `job's: ${jl.retries}` : "0"}
          problem={`A whole number from 0 to ${MAX_RETRIES}`}
          onCommit={(v) => set({ retries: v })} onLeave={leave("retries")}
        />
      </div>
    </section>
  );
}
