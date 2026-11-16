// Pure helpers for the job editor (graph edits, names, statuses). Tested
// without a UI.
import type { Job, JobEdge, JobNode, JobNodeKind, JobRun, NodeRunSummary, OutputInfo } from "./types";
import { uid } from "./util";
import { outputNames, renameOutputRefs } from "./sqlText";

export const NODE_W = 220;
export const NODE_H = 76;
/** Vertical gap when a step is added below another. */
const GAP_Y = 70;

/** Output names: SQL identifiers (`results.<name>`), max 63, no `__` (versions), not a handle (`r12`). */
export const validName = (n: string) => /^[A-Za-z_][A-Za-z0-9_]*$/.test(n) && n.length <= 63 && !n.includes("__") && !/^r\d+$/i.test(n);

/** Why `n` can't be a step name (format only), or null. */
export function nameProblem(n: string): string | null {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(n)) return "Letters, digits and _ only, not starting with a digit (it's the output name)";
  if (n.length > 63) return "At most 63 characters";
  if (n.includes("__")) return '"__" is used for versions (sales__1)';
  if (/^r\d+$/i.test(n)) return "Names like r12 are output handles";
  return null;
}

/**
 * `base`, or `base_2`, `base_3`… not used by another step of the job nor by
 * `taken` (other jobs' steps, named outputs; lowercase).
 */
export function uniqueName(nodes: JobNode[], base: string, except?: string, taken: Iterable<string> = []): string {
  let clean = base.replace(/[^A-Za-z0-9_]+/g, "_").replace(/_{2,}/g, "_").replace(/^([0-9])/, "_$1").replace(/^_+$/, "").slice(0, 56) || "step";
  if (/^r\d+$/i.test(clean)) clean += "_out";
  const used = new Set([...nodes.filter((n) => n.id !== except).map((n) => n.name.toLowerCase()), ...[...taken].map((t) => t.toLowerCase())]);
  if (!used.has(clean.toLowerCase())) return clean;
  // `sales_` + `_2` would be `sales__2`, which reads as version 2 of `sales`
  // (and the editor refuses it): the suffix goes on the name without its trailing `_`.
  const stem = clean.replace(/_+$/, "") || "step";
  for (let i = 2; ; i++) if (!used.has(`${stem}_${i}`.toLowerCase())) return `${stem}_${i}`;
}

export const upstreamOf = (job: Job, id: string) => job.edges.filter((e) => e.to === id).map((e) => e.from);
export const downstreamOf = (job: Job, id: string) => job.edges.filter((e) => e.from === id).map((e) => e.to);

/** Every step `id` depends on, directly or not. */
export function ancestors(job: Job, id: string): Set<string> {
  const out = new Set<string>();
  const walk = (n: string) => upstreamOf(job, n).forEach((u) => !out.has(u) && (out.add(u), walk(u)));
  walk(id);
  return out;
}

/** Every step that depends on `id`. */
export function descendants(job: Job, id: string): Set<string> {
  const out = new Set<string>();
  const walk = (n: string) => downstreamOf(job, n).forEach((d) => !out.has(d) && (out.add(d), walk(d)));
  walk(id);
  return out;
}

/** Why `from → to` can't be added, or null. */
export function edgeProblem(job: Job, from: string, to: string): string | null {
  if (from === to) return "A step can't depend on itself";
  if (job.edges.some((e) => e.from === from && e.to === to)) return "These steps are already linked";
  if (from !== to && ancestors(job, from).has(to)) return "That would make a loop";
  return null;
}

export function addEdge(job: Job, from: string, to: string): Job {
  return edgeProblem(job, from, to) ? job : { ...job, edges: [...job.edges, { from, to }] };
}

export function removeEdge(job: Job, e: JobEdge): Job {
  return { ...job, edges: job.edges.filter((x) => !(x.from === e.from && x.to === e.to)) };
}

export function removeNode(job: Job, id: string): Job {
  return { ...job, nodes: job.nodes.filter((n) => n.id !== id), edges: job.edges.filter((e) => e.from !== id && e.to !== id) };
}

/** DuckDB SQL reading upstream outputs. */
export function readSql(upstreams: string[]): string {
  if (!upstreams.length) return "";
  return `select *\nfrom results.${upstreams[0]}${upstreams.slice(1).map((u) => `\n-- join results.${u} …`).join("")}`;
}

/** A new step; its name is made unique (in the job and against `taken`). */
export function newNode(job: Job, init: Partial<JobNode> & { x: number; y: number }, taken: Iterable<string> = []): JobNode {
  const kind: JobNodeKind = init.kind ?? "query";
  return {
    id: uid(),
    kind,
    connection_id: null,
    sql: "",
    target_connection_id: null,
    target_table: null,
    load_mode: "append",
    export_folder: null,
    export_file: null,
    export_format: "csv",
    last_run: null,
    ...init,
    name: uniqueName(job.nodes, init.name ?? (kind === "query" ? "step" : kind), undefined, taken),
  };
}

/** A free spot below `from` (beside its other downstream steps). */
function spotBelow(job: Job, from: JobNode): { x: number; y: number } {
  const y = from.y + NODE_H + GAP_Y;
  let x = from.x;
  const overlaps = (x: number) => job.nodes.some((n) => Math.abs(n.x - x) < NODE_W + 10 && Math.abs(n.y - y) < NODE_H + 10);
  for (let i = 0; i < 50 && overlaps(x); i++) x += NODE_W + 30;
  return { x, y };
}

/**
 * Add a step below `fromId` that reads its output: a DuckDB query (default)
 * or a load into another connection.
 */
export function addDownstream(job: Job, fromId: string, kind: JobNodeKind, init: Partial<JobNode> = {}, taken: Iterable<string> = []): { job: Job; id: string } {
  const from = job.nodes.find((n) => n.id === fromId);
  if (!from) return { job, id: "" };
  const node = newNode(job, {
    kind,
    name: kind === "query" ? `${from.name}_next` : `${kind}_${from.name}`,
    // Load/export: empty = all rows of this step.
    sql: kind === "query" ? readSql([from.name]) : "",
    target_table: kind === "load" ? from.name : null,
    ...spotBelow(job, from),
    ...init,
  }, taken);
  return { job: { ...job, nodes: [...job.nodes, node], edges: [...job.edges, { from: fromId, to: node.id }] }, id: node.id };
}

/** Status shown on a step: the current/last run of the job, else the step's own last run. */
export function nodeStatus(node: JobNode, run: JobRun | null | undefined): NodeRunSummary | null {
  const r = run?.nodes.find((x) => x.node_id === node.id);
  if (r) return r;
  if (run?.status === "running") return { status: "pending", finished_at: 0, duration_ms: 0 };
  return node.last_run ?? null;
}

/** Steps whose SQL mentions `results.<name>` of a step that isn't upstream (likely a missing link). */
export function missingLinks(job: Job): { node: string; reads: string }[] {
  const out: { node: string; reads: string }[] = [];
  for (const n of job.nodes) {
    const ups = new Set(upstreamOf(job, n.id));
    for (const read of outputNames(n.sql)) {
      const other = job.nodes.find((x) => x.name.toLowerCase() === read.toLowerCase());
      if (other && other.id !== n.id && !ups.has(other.id) && !out.some((o) => o.node === n.id && o.reads === other.id)) out.push({ node: n.id, reads: other.id });
    }
  }
  return out;
}

/**
 * Steps that read a capped output from outside the job (`results.x` of a
 * query tab that stopped at its row limit or memory): the run refuses them
 * before they start, so they'd fail. As the runner checks: steps on DuckDB
 * (a query step without a connection, every load and export step).
 */
export function cappedReads(job: Job, outputs: OutputInfo[], resultsConnectionId?: string | null): { node: string; reads: string; rows: number }[] {
  const out: { node: string; reads: string; rows: number }[] = [];
  const steps = new Set(job.nodes.map((n) => n.name.toLowerCase()));
  for (const n of job.nodes) {
    const onDuckdb = n.kind !== "query" || !n.connection_id || n.connection_id === resultsConnectionId;
    if (!onDuckdb) continue;
    for (const read of outputNames(n.sql)) {
      const name = read.toLowerCase();
      if (steps.has(name) || out.some((o) => o.node === n.id && o.reads.toLowerCase() === name)) continue;
      const o = outputs.find((o) => o.name?.toLowerCase() === name) ?? outputs.find((o) => o.handle.toLowerCase() === name);
      if (o?.truncated) out.push({ node: n.id, reads: read, rows: o.rows });
    }
  }
  return out;
}

const TRIGGER: Record<string, string> = { manual: "manual", schedule: "scheduled", ai: "by AI", resume: "resumed", background: "background", cli: "command line" };

/** How a run was started, short: "scheduled", "after Extract". */
export function triggerText(t: string): string {
  return t.startsWith("after:") ? `after ${t.slice(6)}` : (TRIGGER[t] ?? t);
}

/** Times of day of a daily or monthly schedule, sorted ("HH:MM"). */
export function scheduleTimes(s: Job["schedule"]): string[] {
  return [...new Set(s.times?.length ? s.times : [s.at])].sort();
}

/** "1st", "2nd", "23rd", "last day". */
export function monthDayText(d: number): string {
  if (d === -1) return "last day";
  const suffix = d % 10 === 1 && d !== 11 ? "st" : d % 10 === 2 && d !== 12 ? "nd" : d % 10 === 3 && d !== 13 ? "rd" : "th";
  return `${d}${suffix}`;
}

/** Days of the month in order, the last day after the 31st. */
function sortMonthDays(days: number[]): number[] {
  return [...days].sort((a, b) => (a === -1 ? 99 : a) - (b === -1 ? 99 : b));
}

/** Turn a day of the month (1–31, -1 = last) on or off; at least one stays on. */
export function toggleMonthDay(days: number[], d: number): number[] {
  const cur = days.length ? days : [1];
  const next = cur.includes(d) ? cur.filter((x) => x !== d) : [...cur, d];
  return next.length ? sortMonthDays(next) : cur;
}

/**
 * "every 15 min", "daily at 08:00", "Mon, Fri at 06:30, 18:00", "monthly
 * on the 1st, last day at 05:00", "cron 30 6 * * 1-5", with the time zone
 * when one is set, and "after Extract" for jobs it runs after.
 */
export function scheduleText(s: Job["schedule"], jobName?: (id: string) => string | undefined): string {
  const after = (s.after_jobs ?? []).map((id) => jobName?.(id)).filter((n): n is string => !!n);
  const afterText = after.length ? `after ${after.join(" or ")}` : (s.after_jobs?.length ?? 0) > 0 ? "after another job" : "";
  if (!s.enabled) return afterText || "Not scheduled";
  const tz = s.time_zone?.trim() ? ` (${s.time_zone.trim()})` : "";
  let text: string;
  if (s.mode === "interval") {
    const m = Math.max(1, s.minutes);
    text = m % 1440 === 0 ? `every ${m / 1440} day${m > 1440 ? "s" : ""}` : m % 60 === 0 ? `every ${m / 60} h` : `every ${m} min`;
  } else if (s.mode === "cron") {
    text = `cron ${s.cron?.trim() || "(empty)"}`;
  } else if (s.mode === "monthly") {
    const days = s.month_days?.length ? s.month_days : [1];
    text = `monthly on the ${sortMonthDays(days).map(monthDayText).join(", ")} at ${scheduleTimes(s).join(", ")}`;
  } else {
    const days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const wd = [...s.weekdays].sort((a, b) => ((a + 6) % 7) - ((b + 6) % 7));
    text = `${wd.length === 0 || wd.length === 7 ? "daily" : wd.map((d) => days[d]).join(", ")} at ${scheduleTimes(s).join(", ")}`;
  }
  return `${text}${tz}${afterText ? `, and ${afterText}` : ""}`;
}

/** Sign-in methods that need someone at the computer (browser or device code). */
const INTERACTIVE_SIGN_IN = ["oauth_browser", "device_code", "external_browser"];

/**
 * Names of the job's connections (its steps' and load targets') that sign
 * in through the browser: unattended runs fail once that sign-in ends.
 */
export function browserSignInConnections(job: Job, connections: { id: string; name: string; config: { auth: { method: string } } }[]): string[] {
  const ids = new Set(job.nodes.flatMap((n) => [n.connection_id, n.target_connection_id]).filter((id): id is string => !!id));
  return connections.filter((c) => ids.has(c.id) && INTERACTIVE_SIGN_IN.includes(c.config.auth.method)).map((c) => c.name);
}

/**
 * Change a step. Renaming it also renames `results.<old>` in the SQL of the
 * steps that read it (its downstream steps).
 */
export function updateStep(job: Job, id: string, patch: Partial<JobNode>): Job {
  const old = job.nodes.find((n) => n.id === id)?.name;
  const renamed = patch.name !== undefined && old !== undefined && patch.name !== old;
  const to = patch.name;
  const reads = new Set(downstreamOf(job, id));
  return {
    ...job,
    nodes: job.nodes.map((n) => {
      if (n.id === id) return { ...n, ...patch };
      if (renamed && to !== undefined && reads.has(n.id)) return { ...n, sql: renameOutputRefs(n.sql, old, to) };
      return n;
    }),
  };
}

/** File name an export step writes (same rules as the app; `{date}`/`{time}` shown as placeholders). */
export function exportFileName(template: string | null | undefined, step: string, job: string, format: string, now = new Date()): string {
  const p = (n: number) => String(n).padStart(2, "0");
  const t = template?.trim() || "{step}_{date}_{time}";
  let name = t
    .replaceAll("{step}", step)
    .replaceAll("{job}", job)
    .replaceAll("{date}", `${now.getFullYear()}-${p(now.getMonth() + 1)}-${p(now.getDate())}`)
    .replaceAll("{time}", `${p(now.getHours())}${p(now.getMinutes())}${p(now.getSeconds())}`);
  // eslint-disable-next-line no-control-regex -- control characters aren't allowed in file names
  name = name.replace(/[/\\:*?"<>|\x00-\x1f]/g, "_").trim().replace(/^\.+/, "") || step;
  return name.toLowerCase().endsWith(`.${format}`) ? name : `${name}.${format}`;
}

/** A notebook's SQL cells, enough to build a job from them. */
export interface CellSource {
  source: string;
  output_name?: string | null;
  connection_id?: string | null;
}

/**
 * Job steps from a notebook's SQL cells, top to bottom, each running after
 * the one above. `names[i]` is the step name of cell `i` (empty cells are
 * skipped); `results.<output name>` of an earlier cell is renamed to its
 * step's name. `resultsConn`: the Results DuckDB (steps on it get no
 * connection, which means DuckDB).
 */
export function stepsFromCells(cells: CellSource[], names: string[], defaultConn: string | null, resultsConn?: string | null): { nodes: JobNode[]; edges: JobEdge[] } {
  const nodes: JobNode[] = [];
  const renames: [string, string][] = [];
  cells.forEach((c, i) => {
    if (!c.source.trim()) return;
    let sql = c.source;
    for (const [from, to] of renames) sql = renameOutputRefs(sql, from, to);
    const conn = c.connection_id ?? defaultConn;
    nodes.push({
      id: uid(),
      name: names[i],
      kind: "query",
      connection_id: conn && conn !== resultsConn ? conn : null,
      sql,
      x: 80,
      y: 60 + nodes.length * (NODE_H + GAP_Y),
      load_mode: "append",
      export_folder: null,
      export_file: null,
      export_format: "csv",
      last_run: null,
    });
    const out = c.output_name?.trim();
    if (out && out.toLowerCase() !== names[i].toLowerCase()) renames.push([out, names[i]]);
  });
  const edges = nodes.slice(1).map((n, i) => ({ from: nodes[i].id, to: n.id }));
  return { nodes, edges };
}

/** Step name a cell starts from: its output name, else `{notebook}_{n}`. */
export function cellStepBase(c: CellSource, notebook: string, n: number): string {
  return c.output_name?.trim() || `${notebook.toLowerCase()}_${n}`;
}

/** Comma-separated key columns of a load step → list (empty items dropped). */
export function parseKeyColumns(text: string): string[] {
  return text
    .split(",")
    .map((k) => k.trim())
    .filter(Boolean);
}

/** What each action of a step does (a step does exactly one). */
export const STEP_ACTIONS: { kind: JobNodeKind; title: string; hint: string }[] = [
  { kind: "query", title: "Run a query", hint: "Run SQL on a connection; the result becomes this step's output for later steps." },
  { kind: "load", title: "Load into a connection", hint: "Write the upstream rows into a table of a connection (insert, replace, update or merge)." },
  { kind: "export", title: "Save to a file", hint: "Write the upstream rows to a CSV, Parquet or JSON file." },
];

/** Settings of the step's current action that switching to another one would remove. */
export function actionSettings(node: JobNode): string[] {
  const out: string[] = [];
  if (node.kind === "query") {
    if (node.sql.trim()) out.push("its SQL");
    if (node.connection_id) out.push("its connection");
  } else if (node.kind === "load") {
    if (node.target_connection_id || node.target_table?.trim()) out.push("the target connection and table");
    if (node.load_before_sql?.trim() || node.load_after_sql?.trim()) out.push("the before/after SQL");
    if (node.key_columns?.length) out.push("the key columns");
  } else {
    if (node.export_folder?.trim() || node.export_file?.trim()) out.push("the folder and file name");
  }
  return out;
}

/**
 * Patch that turns a step into `kind`, clearing everything that belonged to
 * its previous action (the app does the same on save). Load and export
 * steps start with "all rows of the upstream step".
 */
export function changeAction(node: JobNode, kind: JobNodeKind, upstreams: string[]): Partial<JobNode> {
  const reset: Partial<JobNode> = {
    kind,
    connection_id: null,
    target_connection_id: null,
    target_table: null,
    load_mode: "append",
    load_before_sql: null,
    load_after_sql: null,
    key_columns: [],
    create_table: true,
    batch_rows: null,
    batch_kb: null,
    load_insert_only: false,
    export_folder: null,
    export_file: null,
    export_format: "csv",
  };
  if (kind === node.kind) return {};
  if (kind === "query") return { ...reset, sql: upstreams.length ? readSql(upstreams) : "" };
  return { ...reset, sql: "", ...(kind === "load" ? { target_table: upstreams[0] ?? node.name } : {}) };
}

/** A load/export step's SQL only repeats the default (all rows of the first upstream step). */
export function usesDefaultRows(node: JobNode, upstreams: string[]): boolean {
  const t = node.sql.trim().replace(/\s+/g, " ").toLowerCase();
  if (!t) return true;
  return !!upstreams.length && t === `select * from results.${upstreams[0].toLowerCase()}`;
}

/**
 * A batch-size box's text → value for the step: empty or 0 = default
 * (`null`), above `max` = `max`. Whole numbers only.
 */
export function batchValue(text: string, max: number): number | null {
  const n = Math.floor(Number(text.trim()));
  if (!text.trim() || !Number.isFinite(n) || n <= 0) return null;
  return Math.min(n, max);
}

const CRON_MONTHS = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const CRON_DAYS = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const CRON_MACROS: Record<string, string> = {
  "@hourly": "0 * * * *",
  "@daily": "0 0 * * *",
  "@midnight": "0 0 * * *",
  "@weekly": "0 0 * * 0",
  "@monthly": "0 0 1 * *",
  "@yearly": "0 0 1 1 *",
  "@annually": "0 0 1 1 *",
};
const U32_MAX = 4_294_967_295;

/** Bits of one cron field (as the scheduler reads it), or what's wrong. */
function cronField(f: string, lo: number, hi: number, names: string[], what: string): Set<number> | string {
  const bits = new Set<number>();
  const value = (v: string): number | string => {
    const i = names.indexOf(v.toLowerCase());
    const n = i >= 0 ? i + (names.length === 12 ? 1 : 0) : /^\d+$/.test(v) ? Number(v) : NaN;
    if (Number.isNaN(n)) return `${what}: "${v}" is not a number`;
    if (n < lo || n > hi) return `${what}: ${n} is out of range (${lo}–${hi})`;
    return n;
  };
  for (const part of f.split(",")) {
    const [range, stepText] = part.includes("/") ? [part.slice(0, part.indexOf("/")), part.slice(part.indexOf("/") + 1)] : [part, null];
    let step = 1;
    if (stepText != null) {
      if (!/^\d+$/.test(stepText) || Number(stepText) < 1 || Number(stepText) > U32_MAX) return `${what}: bad step "${stepText}"`;
      step = Number(stepText);
    }
    let a: number | string, b: number | string;
    let named = false;
    if (range === "*") [a, b] = [lo, hi];
    else if (range.includes("-")) {
      const [x, y] = [range.slice(0, range.indexOf("-")), range.slice(range.indexOf("-") + 1)];
      named = /[a-z]/i.test(x + y);
      [a, b] = [value(x), value(y)];
    } else {
      a = value(range);
      b = stepText != null ? hi : a;
    }
    if (typeof a === "string") return a;
    if (typeof b === "string") return b;
    // Days of the week by name may go round the week ("fri-sun"); numbers stay in order.
    let seq: number[];
    if (a <= b) seq = Array.from({ length: b - a + 1 }, (_, k) => a + k);
    else if (named && names.length === 7) seq = [...Array.from({ length: 7 - a }, (_, k) => a + k), ...Array.from({ length: b + 1 }, (_, k) => k)];
    else return `${what}: range ${a}-${b} is backwards`;
    seq.forEach((i, k) => {
      if (k % step === 0) bits.add(i);
    });
  }
  return bits;
}

/** Longest wait in days between two days matching all of `days`, `months` and `dows` over the calendar's 400-year cycle. */
function cronLongestWait(days: Set<number>, months: Set<number>, dows: Set<number>): number {
  const cycle = 146097;
  let [y, m, d, wd] = [2000, 1, 1, 6]; // 2000-01-01 was a Saturday
  let [first, last, most] = [-1, -1, 0];
  const monthLen = (yy: number, mm: number) => (mm === 2 ? ((yy % 4 === 0 && yy % 100 !== 0) || yy % 400 === 0 ? 29 : 28) : [4, 6, 9, 11].includes(mm) ? 30 : 31);
  for (let k = 0; k < cycle; k++) {
    if (days.has(d) && months.has(m) && dows.has(wd)) {
      if (last >= 0) most = Math.max(most, k - last);
      if (first < 0) first = k;
      last = k;
    }
    wd = (wd + 1) % 7;
    if (++d > monthLen(y, m)) {
      d = 1;
      if (++m > 12) {
        m = 1;
        y++;
      }
    }
  }
  return first < 0 ? Infinity : Math.max(most, first + cycle - last);
}

/**
 * What's wrong with a cron expression, or null: the scheduler's own rules
 * (5 fields, ranges, steps, names, `@daily`…, and a day that can ever come,
 * e.g. not `0 0 30 2 *`), so a bad one is said at the field instead of
 * refusing the whole job's save.
 */
export function cronProblem(expr: string | null | undefined): string | null {
  const e = (expr ?? "").trim();
  if (!e) return "Type a cron expression";
  const f = (CRON_MACROS[e.toLowerCase()] ?? e).split(/\s+/);
  if (f.length !== 5) return `A cron expression has 5 fields (minute hour day month weekday), this has ${f.length}`;
  const parts = [cronField(f[0], 0, 59, [], "minute"), cronField(f[1], 0, 23, [], "hour"), cronField(f[2], 1, 31, [], "day of month"), cronField(f[3], 1, 12, CRON_MONTHS, "month"), cronField(f[4], 0, 7, CRON_DAYS, "day of week")];
  const bad = parts.find((p): p is string => typeof p === "string");
  if (bad) return bad;
  const [, , days, months, dows] = parts as Set<number>[];
  // With a starred day field a day must match both: it must exist in a chosen month…
  if (f[2].startsWith("*") || f[4].startsWith("*")) {
    const len = (m: number) => (m === 2 ? 29 : [4, 6, 9, 11].includes(m) ? 30 : 31);
    if (![...months].some((m) => [...days].some((d) => d <= len(m)))) return "No month has that day: this cron never runs";
    // …and come often enough: the scheduler looks 8 years ahead (Feb 29 on a Sunday waits up to 28).
    if (days.size < 31) {
      const wait = cronLongestWait(days, months, new Set([...dows].map((d) => d % 7)));
      if (wait > 366 * 8) return `Runs too rarely: it can wait ${Math.floor(wait / 365)} years between runs (at most 8)`;
    }
  }
  return null;
}

/** What's wrong with a time zone as the scheduler reads it (empty/"local", UTC, ±hh:mm, an IANA name), or null. */
export function timeZoneProblem(tz: string | null | undefined): string | null {
  const t = (tz ?? "").trim();
  if (!t || /^(local|utc|gmt)$/i.test(t)) return null;
  const off = /^(?:UTC|GMT)?([+-])(.*)$/i.exec(t);
  if (off) {
    const [h, m = "0"] = off[2].split(":");
    const num = (x: string) => (/^\d{1,2}$/.test(x) ? Number(x) : NaN);
    return num(h) <= 14 && num(m) < 60 ? null : `"${t}" is not an offset like +05:30`;
  }
  try {
    new Intl.DateTimeFormat("en", { timeZone: t });
    return null;
  } catch {
    return `Unknown time zone "${t}": an IANA name like Europe/Paris, UTC, or an offset like +05:30`;
  }
}

/** Jobs `jobId` can't run after: those that (through their own "run after") already run after it, and itself. */
export function afterLoopJobs(jobId: string, jobs: { id: string; schedule: { after_jobs?: string[] } }[]): Set<string> {
  // A job J runs after jobId (directly or through others) when jobId is reachable from J's after_jobs.
  const byId = new Map(jobs.map((j) => [j.id, j.schedule.after_jobs ?? []]));
  const reaches = (from: string, seen = new Set<string>()): boolean => {
    if (from === jobId) return true;
    if (seen.has(from)) return false;
    seen.add(from);
    return (byId.get(from) ?? []).some((x) => reaches(x, seen));
  };
  return new Set(jobs.filter((j) => j.id === jobId || reaches(j.id)).map((j) => j.id));
}

/** Whether two schedules run the same (fields the editor sets; defaults filled in). */
export function sameSchedule(a: Job["schedule"] | null | undefined, b: Job["schedule"] | null | undefined): boolean {
  if (!a || !b) return false;
  const norm = (s: Job["schedule"]) =>
    JSON.stringify([
      s.enabled,
      s.mode,
      s.minutes,
      scheduleTimes(s),
      [...(s.weekdays ?? [])].sort(),
      [...(s.month_days ?? [])].sort(),
      (s.cron ?? "").trim(),
      (s.time_zone ?? "").trim(),
      s.missed ?? "once",
      [...(s.after_jobs ?? [])].sort(),
    ]);
  return norm(a) === norm(b);
}

/** A step's fields that decide whose SQL it is (as the app's `JobNode::same_sql`). */
const TEXT_FIELDS = ["kind", "sql", "connection_id", "target_connection_id", "load_before_sql", "load_after_sql"] as const;
/** Fields the app keeps, not the editor. */
const SERVER_FIELDS = new Set(["last_run", "updated_at", "created_at", "last_scheduled_at", "id"]);

const same = (a: unknown, b: unknown) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
const text = (v: unknown) => (typeof v === "string" ? v.trim() : (v ?? null));
const sameText = (a: JobNode, b: JobNode) => TEXT_FIELDS.every((k) => same(text(a[k]), text(b[k])));
/** Where and how a step writes (as the app's `JobNode::same_effect`, with its SQL). */
const EFFECT_FIELDS = ["target_table", "load_mode", "key_columns", "create_table", "export_folder", "export_file", "export_format"] as const;
const sameEffect = (a: JobNode, b: JobNode) => sameText(a, b) && EFFECT_FIELDS.every((k) => same(text(a[k]), text(b[k])));

/**
 * Steps whose SQL was edited here while the assistant changed the same step
 * (its SQL, connections or where it writes) in the version saved since: the
 * rebase keeps the assistant's version of those (see [`rebaseJob`]).
 */
export function rebaseClashes(base: Job, mine: Job, theirs: Job): string[] {
  const baseById = new Map(base.nodes.map((n) => [n.id, n]));
  const mineById = new Map(mine.nodes.map((n) => [n.id, n]));
  return theirs.nodes
    .filter((t) => {
      const b = baseById.get(t.id);
      const m = mineById.get(t.id);
      return !!b && !!m && !sameText(m, b) && !sameEffect(t, b);
    })
    .map((t) => t.name);
}

/**
 * Edits made on `base` (`mine`) put on top of the job as saved since
 * (`theirs`, e.g. after the assistant changed it): what the user changed
 * wins, what they left alone is `theirs`. A step's SQL, connections and
 * before/after SQL go together: the user's version when they edited any of
 * them (the step is then theirs, as any step they edit), else `theirs`
 * whole, with its AI mark — so an older version of SQL the assistant wrote
 * never comes back as the user's. Clearing the mark ("Trust this SQL")
 * only counts for the SQL the user trusted. A step both changed (the user
 * its SQL, the version since its SQL, connections or where it writes) keeps
 * the version since: the user is told ([`rebaseClashes`]) and edits again.
 */
export function rebaseJob(base: Job, mine: Job, theirs: Job): Job {
  const out: Job = { ...theirs };
  for (const k of Object.keys(mine) as (keyof Job)[]) {
    if (k === "nodes" || k === "edges" || SERVER_FIELDS.has(k)) continue;
    if (!same(mine[k], base[k])) (out as Record<string, unknown>)[k] = mine[k];
  }
  const baseById = new Map(base.nodes.map((n) => [n.id, n]));
  const mineById = new Map(mine.nodes.map((n) => [n.id, n]));
  const theirIds = new Set(theirs.nodes.map((n) => n.id));
  const nodes: JobNode[] = [];
  for (const t of theirs.nodes) {
    const b = baseById.get(t.id);
    const m = mineById.get(t.id);
    if (!b || !m) {
      // Added since (theirs), or deleted by the user (in base, not in mine).
      if (!b) nodes.push(t);
      continue;
    }
    const n: Record<string, unknown> = { ...t };
    for (const k of Object.keys(m) as (keyof JobNode)[]) {
      if (SERVER_FIELDS.has(k) || (TEXT_FIELDS as readonly string[]).includes(k) || k === "ai_sql") continue;
      if (!same(m[k], b[k])) n[k] = m[k];
    }
    if (!sameText(m, b) && !sameEffect(t, b)) {
      // Both changed the step: the version saved since stays whole (SQL,
      // connections, where it writes, its mark), so the user's SQL can't
      // take what the assistant changed as the user's ([`rebaseClashes`]).
      for (const k of [...TEXT_FIELDS, ...EFFECT_FIELDS]) n[k] = t[k];
      n.ai_sql = t.ai_sql;
    } else if (!sameText(m, b)) {
      for (const k of TEXT_FIELDS) n[k] = m[k];
      n.ai_sql = m.ai_sql;
    } else if (!same(m.ai_sql, b.ai_sql) && sameText(t, b)) {
      n.ai_sql = m.ai_sql;
    }
    nodes.push(n as JobNode);
  }
  // Steps the user added.
  for (const m of mine.nodes) if (!baseById.has(m.id) && !theirIds.has(m.id)) nodes.push(m);
  const ids = new Set(nodes.map((n) => n.id));
  const key = (e: JobEdge) => `${e.from}\u0000${e.to}`;
  const baseEdges = new Set(base.edges.map(key));
  const mineEdges = new Set(mine.edges.map(key));
  const edges = theirs.edges.filter((e) => !(baseEdges.has(key(e)) && !mineEdges.has(key(e))));
  const have = new Set(edges.map(key));
  for (const e of mine.edges) if (!baseEdges.has(key(e)) && !have.has(key(e))) edges.push(e);
  out.nodes = nodes;
  out.edges = edges.filter((e) => ids.has(e.from) && ids.has(e.to));
  return out;
}
