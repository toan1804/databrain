// `:run_date` / `:run_time`: the date (and time) a job run is for, filled
// by the runner (see RUN_DATE in crates/app/src/jobs.rs), not job parameters.

export const RUN_DATE = "run_date";
export const RUN_TIME = "run_time";

/** A parameter the runner fills itself. */
export const isRunMoment = (name: string) => {
  const n = name.toLowerCase();
  return n === RUN_DATE || n === RUN_TIME;
};

/** Today in the computer's time zone, `2026-11-16`. */
export function today(now = new Date()): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `${now.getFullYear()}-${p(now.getMonth() + 1)}-${p(now.getDate())}`;
}

// The current date or time, as databases spell it.
const WALL_CLOCK = /\b(?:(current_date|current_timestamp|localtimestamp|systimestamp|sysdate)\b|(getdate|getutcdate|sysdatetime|now|curdate|today)\s*\()/i;

/**
 * The current date or time `sql` reads (`current_date`, `now()`, …), outside
 * strings, quoted names and comments, or null. A late run, a missed run made
 * up later or a Resume the next day reads another day with it.
 */
export function wallClockUse(sql: string): string | null {
  const code = sql
    .replace(/--[^\n]*|\/\*[\s\S]*?\*\//g, " ")
    .replace(/'(?:[^']|'')*'|"(?:[^"]|"")*"|`[^`]*`|\[[^\]]*\]/g, " ");
  const m = WALL_CLOCK.exec(code);
  if (!m) return null;
  return (m[1] ?? `${m[2]}()`).toLowerCase();
}
