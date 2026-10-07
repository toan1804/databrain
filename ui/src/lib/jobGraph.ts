// Pure helpers for the job editor (graph edits, names, statuses). Tested
// without a UI.
import type { Job, JobEdge, JobNode, JobNodeKind, JobRun, NodeRunSummary } from "./types";
import { uid } from "./util";

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
  for (let i = 2; ; i++) if (!used.has(`${clean}_${i}`.toLowerCase())) return `${clean}_${i}`;
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
    sql: readSql([from.name]),
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
    for (const m of n.sql.matchAll(/\bresults\s*\.\s*"?([A-Za-z_][A-Za-z0-9_]*)"?/gi)) {
      const other = job.nodes.find((x) => x.name.toLowerCase() === m[1].toLowerCase());
      if (other && other.id !== n.id && !ups.has(other.id) && !out.some((o) => o.node === n.id && o.reads === other.id)) out.push({ node: n.id, reads: other.id });
    }
  }
  return out;
}

/** "every 15 min", "daily at 08:00", "Mon, Fri at 06:30". */
export function scheduleText(s: Job["schedule"]): string {
  if (!s.enabled) return "Not scheduled";
  if (s.mode === "interval") {
    const m = Math.max(1, s.minutes);
    return m % 1440 === 0 ? `every ${m / 1440} day${m > 1440 ? "s" : ""}` : m % 60 === 0 ? `every ${m / 60} h` : `every ${m} min`;
  }
  const days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
  const wd = [...s.weekdays].sort((a, b) => ((a + 6) % 7) - ((b + 6) % 7));
  return `${wd.length === 0 || wd.length === 7 ? "daily" : wd.map((d) => days[d]).join(", ")} at ${s.at}`;
}

/**
 * Change a step. Renaming it also renames `results.<old>` in the SQL of the
 * steps that read it (its downstream steps).
 */
export function updateStep(job: Job, id: string, patch: Partial<JobNode>): Job {
  const old = job.nodes.find((n) => n.id === id)?.name;
  const renamed = patch.name !== undefined && old !== undefined && patch.name !== old;
  const re = renamed ? new RegExp(`\\bresults(\\s*\\.\\s*)"?${old}"?(?![A-Za-z0-9_])`, "gi") : null;
  const reads = new Set(downstreamOf(job, id));
  return {
    ...job,
    nodes: job.nodes.map((n) => {
      if (n.id === id) return { ...n, ...patch };
      if (re && reads.has(n.id)) return { ...n, sql: n.sql.replace(re, (_m, dot: string) => `results${dot}${patch.name}`) };
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
  // eslint-disable-next-line no-control-regex
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
  const renames: [RegExp, string][] = [];
  cells.forEach((c, i) => {
    if (!c.source.trim()) return;
    let sql = c.source;
    for (const [re, to] of renames) sql = sql.replace(re, (_m, dot: string) => `results${dot}${to}`);
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
    if (out && out.toLowerCase() !== names[i].toLowerCase()) renames.push([new RegExp(`\\bresults(\\s*\\.\\s*)"?${out}"?(?![A-Za-z0-9_])`, "gi"), names[i]]);
  });
  const edges = nodes.slice(1).map((n, i) => ({ from: nodes[i].id, to: n.id }));
  return { nodes, edges };
}

/** Step name a cell starts from: its output name, else `{notebook}_{n}`. */
export function cellStepBase(c: CellSource, notebook: string, n: number): string {
  return c.output_name?.trim() || `${notebook.toLowerCase()}_${n}`;
}
