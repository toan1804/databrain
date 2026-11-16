// The job's schedule: when it runs (every N minutes, times of day, days of
// the month, cron) in which time zone, what happens to missed runs, and the
// jobs it runs after.
import { useEffect, useId, useRef, useState } from "react";
import { AlertTriangle, CalendarClock, Info, Plus, X } from "lucide-react";
import { api } from "../../lib/api";
import type { Job, JobSchedule, MissedRuns, ScheduleMode } from "../../lib/types";
import { useStore } from "../../store";
import { useJobs } from "../../jobsStore";
import { afterLoopJobs, browserSignInConnections, cronProblem, monthDayText, sameSchedule, scheduleTimes, timeZoneProblem, toggleMonthDay } from "../../lib/jobGraph";
import { field, label, useCommitOnUnmount, type Patch } from "./shared";

/** Most minutes between runs (a year): the backend's `MAX_MINUTES`, refused on save too. */
export const MAX_MINUTES = 525_600;

const WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const ORDER = [1, 2, 3, 4, 5, 6, 0];
const MODES: [ScheduleMode, string][] = [
  ["interval", "Every…"],
  ["daily", "Times of day"],
  ["monthly", "Monthly"],
  ["cron", "Cron"],
];
const MISSED: [MissedRuns, string][] = [
  ["once", "Run once"],
  ["skip", "Skip them"],
  ["each", "Run each (up to 24)"],
];

/** IANA time zones the browser knows (only while the backend's list can't be had). */
function browserTimeZones(): string[] {
  try {
    return (Intl as unknown as { supportedValuesOf?: (k: string) => string[] }).supportedValuesOf?.("timeZone") ?? [];
  } catch {
    return [];
  }
}

/** The time zone names the backend takes (the browser's list differs by system and version). */
let zoneList: Promise<string[]> | null = null;
function useTimeZones(): string[] {
  const [zones, setZones] = useState<string[]>([]);
  useEffect(() => {
    let live = true;
    zoneList ??= api.jobTimeZones().catch(() => {
      zoneList = null;
      return browserTimeZones();
    });
    void zoneList.then((z) => live && setZones(z));
    return () => {
      live = false;
    };
  }, []);
  return zones;
}

/**
 * The backend's answer decides whether a cron or time zone can be saved
 * (the same check a save makes); the local rules stand in only when it
 * can't be asked.
 */
const checkCron = (v: string) => api.checkJobCron(v).catch(() => cronProblem(v));
const checkTimeZone = (v: string) => api.checkJobTimeZone(v).catch(() => timeZoneProblem(v));

export function ScheduleSection({ job, nextRunAt, onChange }: { job: Job; nextRunAt: number | null; onChange: (patch: Patch<Job>) => void }) {
  const s = job.schedule;
  const set = (patch: Partial<JobSchedule>) => onChange({ schedule: { ...s, ...patch } });
  // A field saved when the panel closes: onto the schedule as it is then
  // (the assistant's version may have been shown meanwhile), and only while
  // that field still holds what it was typed over.
  const leave = (unchanged: (cur: JobSchedule) => boolean, patch: (cur: JobSchedule) => Partial<JobSchedule>) =>
    onChange((j) => (unchanged(j.schedule) ? { schedule: { ...j.schedule, ...patch(j.schedule) } } : null));
  const connections = useStore((st) => st.connections);
  const jobs = useJobs((st) => st.jobs);
  const saved = jobs.find((j) => j.id === job.id)?.schedule;
  const zones = useTimeZones();
  const after = s.after_jobs ?? [];
  const signIn = browserSignInConnections(job, connections);
  const times = scheduleTimes(s);
  const setTimes = (next: string[]) => {
    const sorted = [...next].sort();
    if (sorted.length) set({ times: sorted, at: sorted[0] });
  };
  // Cron chosen while it has no valid expression yet: shown, but saved
  // (mode and cron together) only once a valid one is typed, so the job
  // keeps saving its other edits meanwhile.
  const [cronDraft, setCronDraft] = useState(false);
  // The kind the job had when Cron was picked: a cron saved on close sets
  // the kind only if nothing else changed it meanwhile.
  const draftFrom = useRef<ScheduleMode | null>(null);
  useEffect(() => {
    setCronDraft(false);
    draftFrom.current = null;
  }, [job.id]);
  const mode: ScheduleMode = cronDraft ? "cron" : s.mode;
  const pickMode = (m: ScheduleMode) => {
    // Cron is saved as the kind once its expression is (checked when the
    // field is left); a valid one kept from before is saved at once.
    if (m === "cron" && s.mode !== "cron") {
      const from = s.mode;
      const kept = (s.cron ?? "").trim();
      draftFrom.current = from;
      setCronDraft(true);
      if (kept)
        void checkCron(kept).then((why) => {
          if (why || draftFrom.current !== from) return;
          setCronDraft(false);
          draftFrom.current = null;
          onChange((j) => (j.schedule.mode === from && (j.schedule.cron ?? "").trim() === kept ? { schedule: { ...j.schedule, mode: "cron" } } : null));
        });
      return;
    }
    setCronDraft(false);
    draftFrom.current = null;
    if (m !== s.mode) set({ mode: m });
  };
  // Jobs this one can't run after: they (through theirs) already run after it.
  const loops = afterLoopJobs(job.id, jobs);
  const others = jobs.filter((j) => !loops.has(j.id) && !after.includes(j.id));
  // Jobs deleted since are dropped from the list when saved: not a difference.
  const known = new Set(jobs.map((j) => j.id));
  const live = jobs.length ? { ...s, after_jobs: after.filter((id) => known.has(id)) } : s;
  const nextText = !sameSchedule(live, saved) || cronDraft ? "Next run: after saving" : nextRunAt ? `Next run ${new Date(nextRunAt).toLocaleString()}` : "No run due";

  return (
    <section className="space-y-2">
      <div className="flex items-center justify-between">
        <div className={label}>Schedule</div>
        <label className="flex items-center gap-1.5 text-[12px]">
          <input type="checkbox" checked={s.enabled} onChange={(e) => set({ enabled: e.target.checked })} /> Run on a schedule
        </label>
      </div>
      {s.enabled && (
        <div className="space-y-2">
          <div className="flex rounded-md border border-line p-0.5" role="group" aria-label="Schedule kind">
            {MODES.map(([m, text]) => (
              <button key={m} aria-pressed={mode === m} className={`flex-1 rounded px-2 py-1 text-[11.5px] ${mode === m ? "bg-accent/15 font-medium text-accent" : "text-muted hover:text-fg"}`} onClick={() => pickMode(m)}>
                {text}
              </button>
            ))}
          </div>
          {mode === "interval" && (
            <div className="flex items-center gap-2">
              <span>Every</span>
              <MinutesInput
                className={`${field} w-20`}
                value={s.minutes}
                onCommit={(minutes) => set({ minutes })}
                onLeave={(minutes, base) =>
                  leave(
                    (cur) => cur.minutes === base,
                    () => ({ minutes }),
                  )
                }
              />
              <span>min</span>
              <select className={`${field} w-auto`} value="" onChange={(e) => e.target.value && set({ minutes: Number(e.target.value) })} aria-label="Common intervals">
                <option value="">Presets…</option>
                <option value="15">15 min</option>
                <option value="60">1 hour</option>
                <option value="360">6 hours</option>
                <option value="1440">1 day</option>
              </select>
            </div>
          )}
          {(mode === "daily" || mode === "monthly") && (
            <TimesOfDay
              times={times}
              onCommit={setTimes}
              onLeave={(list, base) => {
                const sorted = [...list].sort();
                leave(
                  (cur) => scheduleTimes(cur).join(",") === base.join(","),
                  () => ({ times: sorted, at: sorted[0] }),
                );
              }}
            />
          )}
          {mode === "daily" && (
            <div className="flex gap-1" role="group" aria-label="Days">
              {ORDER.map((d) => {
                const on = s.weekdays.length === 0 || s.weekdays.includes(d);
                return (
                  <button
                    key={d}
                    aria-pressed={on}
                    className={`flex-1 rounded border px-1 py-0.5 text-[11px] ${on ? "border-accent/50 bg-accent/10 text-accent" : "border-line text-muted"}`}
                    onClick={() => {
                      const cur = s.weekdays.length === 0 ? ORDER : s.weekdays;
                      const next = on ? cur.filter((x) => x !== d) : [...cur, d];
                      // An empty list means every day: the last day can't be turned off into that.
                      if (next.length === 0) return;
                      set({ weekdays: next.length === 7 ? [] : next });
                    }}
                  >
                    {WEEKDAYS[d]}
                  </button>
                );
              })}
            </div>
          )}
          {mode === "monthly" && (
            <div className="space-y-1">
              <div className="grid grid-cols-8 gap-0.5" role="group" aria-label="Days of the month">
                {[...Array.from({ length: 31 }, (_, i) => i + 1), -1].map((d) => {
                  const on = (s.month_days?.length ? s.month_days : [1]).includes(d);
                  return (
                    <button
                      key={d}
                      aria-pressed={on}
                      title={d === -1 ? "The last day of each month" : d > 28 ? `Months without a ${monthDayText(d)} are skipped: use Last for the month's end` : undefined}
                      className={`rounded border px-0.5 py-0.5 text-[11px] ${on ? "border-accent/50 bg-accent/10 text-accent" : "border-line text-muted"}`}
                      onClick={() => set({ month_days: toggleMonthDay(s.month_days ?? [], d) })}
                    >
                      {d === -1 ? "Last" : d}
                    </button>
                  );
                })}
              </div>
            </div>
          )}
          {mode === "cron" && (
            <div className="space-y-1">
              <CommitInput
                className={`${field} font-mono`}
                value={s.cron ?? ""}
                placeholder="30 6 * * 1-5"
                ariaLabel="Cron expression"
                check={checkCron}
                onCommit={(cron) => {
                  setCronDraft(false);
                  draftFrom.current = null;
                  set({ mode: "cron", cron });
                }}
                onLeave={(cron, base) => {
                  const from = draftFrom.current;
                  leave(
                    (cur) => (cur.cron ?? "").trim() === base.trim() && (from === null || cur.mode === from),
                    () => (from === null ? { cron } : { mode: "cron", cron }),
                  );
                }}
              />
              <div className="text-[11px] text-muted">minute hour day-of-month month day-of-week, e.g. 30 6 * * 1-5 (weekdays at 06:30), */15 * * * *, 0 0 1 * *, @daily.</div>
            </div>
          )}
          <div className="grid grid-cols-[auto_1fr] items-center gap-x-2 gap-y-1.5">
            <label htmlFor={`tz-${job.id}`} className="text-[12px] text-muted">
              Time zone
            </label>
            <CommitInput
              id={`tz-${job.id}`}
              className={field}
              list={`tz-list-${job.id}`}
              value={s.time_zone ?? ""}
              placeholder="This computer's"
              ariaLabel="Time zone"
              check={checkTimeZone}
              onCommit={(time_zone) => set({ time_zone })}
              onLeave={(time_zone, base) =>
                leave(
                  (cur) => (cur.time_zone ?? "").trim() === base.trim(),
                  () => ({ time_zone }),
                )
              }
            />
            <datalist id={`tz-list-${job.id}`}>
              {["UTC", ...zones].map((z) => (
                <option key={z} value={z} />
              ))}
            </datalist>
            <label htmlFor={`missed-${job.id}`} className="text-[12px] text-muted" title="Runs missed while neither DataBrain nor its background runner was running (the computer was off or asleep)">
              Missed runs
            </label>
            <select id={`missed-${job.id}`} className={field} value={s.missed ?? "once"} onChange={(e) => set({ missed: e.target.value as MissedRuns })}>
              {MISSED.map(([m, text]) => (
                <option key={m} value={m}>
                  {text}
                </option>
              ))}
            </select>
          </div>
          <div
            className="flex items-center gap-1 text-[11.5px] text-muted"
            title="Runs while DataBrain is open (or by the background runner). A time skipped when clocks go forward runs at the first time after it; a time repeated when clocks go back runs once."
          >
            <CalendarClock size={12} />
            {nextText}
            <Info size={11} className="shrink-0" />
          </div>
        </div>
      )}
      <div className="space-y-1">
        <div className="flex flex-wrap items-center gap-1.5" role="group" aria-label="Run after jobs">
          <span className="text-[12px] text-muted">Run after</span>
          {after.map((id) => (
            <span key={id} className="flex items-center gap-0.5 rounded border border-line px-1.5 py-0.5 text-[11.5px]">
              {jobs.find((j) => j.id === id)?.name ?? "a job"}
              <button className="text-muted hover:text-fg" aria-label={`Don't run after ${jobs.find((j) => j.id === id)?.name ?? "this job"}`} onClick={() => set({ after_jobs: after.filter((a) => a !== id) })}>
                <X size={11} />
              </button>
            </span>
          ))}
          {others.length > 0 && (
            <select className={`${field} w-auto`} value="" aria-label="Run after another job" onChange={(e) => e.target.value && set({ after_jobs: [...after, e.target.value] })}>
              <option value="">{after.length ? "or after…" : "another job succeeds…"}</option>
              {others.map((j) => (
                <option key={j.id} value={j.id}>
                  {j.name}
                </option>
              ))}
            </select>
          )}
          {!others.length && !after.length && <span className="text-[11.5px] text-muted">(no other jobs yet)</span>}
        </div>
        {after.length > 0 && <div className="text-[11px] text-muted">Starts when a run of any of these jobs, as a whole, succeeds{s.enabled ? ", and on its schedule" : ""}.</div>}
      </div>
      {(s.enabled || after.length > 0) && signIn.length > 0 && (
        <div className="flex items-start gap-1.5 rounded-md border border-warning/50 bg-warning/5 p-1.5 text-[11.5px]" role="note">
          <AlertTriangle size={12} className="mt-0.5 shrink-0 text-warning" />
          <span>
            {signIn.map((n) => `"${n}"`).join(", ")} sign{signIn.length === 1 ? "s" : ""} in through the browser. Runs nobody watches fail once that sign-in ends, until you sign in again: for scheduled jobs prefer a key, token or service account.
          </span>
        </div>
      )}
    </section>
  );
}

/** A time of day not yet in `times`: an hour after the last one. */
function nextFreeTime(times: string[]): string {
  const last = times.length ? times[times.length - 1] : "08:00";
  let h = Number(last.slice(0, 2));
  for (let i = 0; i < 24; i++) {
    h = (h + 1) % 24;
    const t = `${String(h).padStart(2, "0")}${last.slice(2)}`;
    if (!times.includes(t)) return t;
  }
  return last;
}

/**
 * Times of day, edited in place (kept in the order shown while typed, each
 * saved when left, then sorted): saving each change re-sorted the list and
 * recreated the field being typed in. A time already in the list is said,
 * not silently merged.
 */
function TimesOfDay({ times, onCommit, onLeave }: { times: string[]; onCommit: (times: string[]) => void; onLeave: (times: string[], base: string[]) => void }) {
  const [draft, setDraft] = useState(times);
  const [dup, setDup] = useState<string | null>(null);
  const key = times.join(",");
  // eslint-disable-next-line react-hooks/exhaustive-deps -- by value (key): a new array with the same times keeps the draft
  useEffect(() => setDraft(times), [key]);
  const commit = (list = draft) => {
    const seen = list.find((t, i) => list.indexOf(t) !== i);
    if (seen) return setDup(seen);
    setDup(null);
    if (list.join(",") !== [...times].join(",")) onCommit(list);
  };
  useCommitOnUnmount(() => {
    if (!draft.some((t, i) => draft.indexOf(t) !== i) && draft.join(",") !== times.join(",")) onLeave(draft, times);
  });
  const errId = useId();
  return (
    <div className="space-y-0.5">
      <div className="flex flex-wrap items-center gap-1.5" role="group" aria-label="Times of day">
        <span>At</span>
        {draft.map((t, i) => (
          <span key={i} className={`flex items-center rounded border ${dup === t ? "border-danger" : "border-line"}`}>
            <input
              type="time"
              className="bg-transparent px-1 py-0.5 text-[12px] outline-none"
              value={t}
              aria-label={`Time ${i + 1}`}
              aria-invalid={dup === t || undefined}
              aria-describedby={dup === t ? errId : undefined}
              onChange={(e) => e.target.value && setDraft(draft.map((x, j) => (j === i ? e.target.value : x)))}
              onBlur={() => commit()}
              onKeyDown={(e) => e.key === "Enter" && commit()}
            />
            {draft.length > 1 && (
              <button className="px-1 text-muted hover:text-fg" aria-label={`Remove ${t}`} onClick={() => commit(draft.filter((_, j) => j !== i))}>
                <X size={11} />
              </button>
            )}
          </span>
        ))}
        <button className="flex items-center gap-0.5 rounded px-1 text-[11.5px] text-muted hover:text-fg" onClick={() => commit([...draft, nextFreeTime(draft)])} title="Run at another time of day too">
          <Plus size={11} /> time
        </button>
      </div>
      {dup && (
        <div id={errId} className="text-[11px] text-danger">
          {dup} is already in the list: change or remove one of them.
        </div>
      )}
    </div>
  );
}

/**
 * Text saved on blur or Enter (a half-typed cron or time zone isn't saved),
 * and when the panel closes. `check` asks the backend first: a bad value is
 * said at the field and not sent, so it can't refuse the whole job's save.
 * The saved value is checked too (a cron from before, or one a newer rule
 * refuses), so the field says why the job won't save.
 */
function CommitInput({
  value,
  onCommit,
  onLeave,
  className,
  placeholder,
  ariaLabel,
  id,
  list,
  check,
}: {
  value: string;
  onCommit: (v: string) => void;
  /** Saves what was typed when the panel closes (`base`: the value it was typed over). */
  onLeave: (v: string, base: string) => void;
  className: string;
  placeholder?: string;
  ariaLabel: string;
  id?: string;
  list?: string;
  check: (v: string) => Promise<string | null>;
}) {
  const [text, setText] = useState(value);
  const [bad, setBad] = useState<{ why: string; typed: boolean } | null>(null);
  // Only the latest check counts (an older one answering late).
  const asked = useRef(0);
  useEffect(() => {
    setText(value);
    const n = ++asked.current;
    const v = value.trim();
    if (!v) return setBad(null);
    void check(v).then((why) => n === asked.current && setBad(why ? { why, typed: false } : null));
  }, [value, check]);
  const errId = useId();
  const commit = () => {
    const t = text.trim();
    if (t === value.trim()) {
      // Back to the saved value: its own check stands.
      if (bad?.typed) {
        const n = ++asked.current;
        void check(t).then((why) => n === asked.current && setBad(why && t ? { why, typed: false } : null));
      }
      return;
    }
    const n = ++asked.current;
    void check(t).then((why) => {
      if (n !== asked.current) return;
      setBad(why ? { why, typed: true } : null);
      if (!why) onCommit(t);
    });
  };
  useCommitOnUnmount(() => {
    const t = text.trim();
    if (t !== value.trim()) void check(t).then((why) => !why && onLeave(t, value));
  });
  return (
    <div>
      <input
        id={id}
        list={list}
        className={`${className} ${bad ? "border-danger" : ""}`}
        value={text}
        placeholder={placeholder}
        aria-label={ariaLabel}
        aria-invalid={!!bad || undefined}
        aria-describedby={bad ? errId : undefined}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
        }}
      />
      {bad && (
        <div id={errId} className="mt-0.5 text-[11px] text-danger">
          {bad.why} {bad.typed ? "(not saved)" : "(the job won't save until it's fixed)"}
        </div>
      )}
    </div>
  );
}

/** Minutes typed in the field: a whole number from 1 to [`MAX_MINUTES`], else null (a fraction is not rounded). */
export function parseMinutes(text: string): number | null {
  const t = text.trim();
  if (!/^\d+$/.test(t)) return null;
  const n = Number(t);
  return n >= 1 && n <= MAX_MINUTES ? n : null;
}

/**
 * Minutes between runs, typed freely and saved when it is a number (on
 * blur or Enter, or when the panel closes). Saving every keystroke turned a
 * cleared field into "every 1 minute", which the scheduler then ran.
 */
function MinutesInput({
  value,
  onCommit,
  onLeave,
  className,
}: {
  value: number;
  onCommit: (minutes: number) => void;
  /** Saves what was typed when the panel closes (`base`: the value it was typed over). */
  onLeave: (minutes: number, base: number) => void;
  className: string;
}) {
  const [text, setText] = useState(String(value));
  const [bad, setBad] = useState(false);
  useEffect(() => setText(String(value)), [value]);
  const errId = useId();
  const commit = () => {
    const n = parseMinutes(text);
    setBad(n == null);
    if (n == null) return;
    if (n !== value) onCommit(n);
    setText(String(n));
  };
  useCommitOnUnmount(() => {
    const n = parseMinutes(text);
    if (n != null && n !== value) onLeave(n, value);
  });
  return (
    <span>
      <input
        type="number"
        min={1}
        max={MAX_MINUTES}
        className={`${className} ${bad ? "border-danger" : ""}`}
        value={text}
        aria-label="Minutes"
        aria-invalid={bad || undefined}
        aria-describedby={bad ? errId : undefined}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
        }}
      />
      {bad && (
        <span id={errId} className="ml-1 text-[11px] text-danger">
          a whole number from 1 to {MAX_MINUTES.toLocaleString()} minutes
        </span>
      )}
    </span>
  );
}
