// Jobs list, running jobs and their live runs (from "job-run" events).
import { create } from "zustand";
import { api, isTauri, onJobRun, onJobRunLog, toError } from "./lib/api";
import type { Job, JobRun, JobSummary, RunLogEntry } from "./lib/types";
import { useStore } from "./store";
import { uid } from "./lib/util";
import { cellStepBase, stepsFromCells, uniqueName, type CellSource } from "./lib/jobGraph";

/**
 * A step name no other job's step or named output uses (`step`, `step_2`…).
 * Checked with the app too (notebook cells' output names).
 */
export async function freeStepName(base: string, jobId = "", also: string[] = []): Promise<string> {
  const taken = new Set<string>(also.map((a) => a.toLowerCase()));
  for (const j of useJobs.getState().jobs) if (j.id !== jobId) for (const n of j.step_names ?? []) taken.add(n.toLowerCase());
  for (const o of useStore.getState().outputs) if (o.name && !o.tab_id.startsWith("job:")) taken.add(o.name.toLowerCase());
  for (let i = 0; i < 20; i++) {
    const name = uniqueName([], base, undefined, taken);
    const user = isTauri() ? await api.outputNameUser(name, jobId).catch(() => null) : null;
    if (!user) return name;
    taken.add(name.toLowerCase());
  }
  return uniqueName([], base, undefined, taken);
}

interface JobsState {
  jobs: JobSummary[];
  /** Latest run per job seen in this session (live while running). */
  runs: Record<string, JobRun>;
  running: Record<string, boolean>;
  /** Log lines of runs seen live in this session, by run id. */
  logs: Record<number, RunLogEntry[]>;
  refresh: () => Promise<void>;
  init: () => Promise<void>;
  open: (id: string, name: string) => void;
  create: () => Promise<void>;
  /** A job with one step per SQL cell of a notebook, top to bottom. */
  createFromNotebook: (nb: { name: string; connection_id?: string | null; cells: (CellSource & { kind: string })[] }) => Promise<void>;
  remove: (j: { id: string; name: string }) => void;
  run: (id: string, nodes?: string[]) => Promise<void>;
  cancel: (id: string) => Promise<void>;
}

let started = false;
/** Scheduled runs already announced ("Starting job …"). */
const announced = new Set<number>();
/** Finished runs whose notices were shown. */
const noticed = new Set<number>();

export function announceScheduled(jobs: JobSummary[], run: JobRun) {
  if (run.trigger !== "schedule" || run.status !== "running" || announced.has(run.id)) return;
  announced.add(run.id);
  const name = jobs.find((j) => j.id === run.job_id)?.name ?? "a scheduled job";
  useStore.getState().toast(`Starting scheduled job "${name}"…`, "info");
}
let refreshTimer: ReturnType<typeof setTimeout> | undefined;

export const useJobs = create<JobsState>((set, get) => ({
  jobs: [],
  runs: {},
  running: {},
  logs: {},
  refresh: async () => {
    if (!isTauri()) return;
    const { jobs, running } = await api.listJobs();
    set({ jobs, running: Object.fromEntries(running.map((id) => [id, true])) });
  },
  init: async () => {
    if (started || !isTauri()) return;
    started = true;
    await onJobRunLog(({ run_id, entries }) =>
      set((s) => {
        const cur = s.logs[run_id] ?? [];
        const last = cur.length ? cur[cur.length - 1].seq : 0;
        const fresh = entries.filter((e) => e.seq > last);
        return fresh.length ? { logs: { ...s.logs, [run_id]: [...cur, ...fresh] } } : {};
      }),
    );
    await onJobRun(({ job_id, run }) => {
      set((s) => ({ runs: { ...s.runs, [job_id]: run }, running: { ...s.running, [job_id]: run.status === "running" } }));
      announceScheduled(get().jobs, run);
      if (run.status !== "running") {
        // Tell what a step did that the user didn't set up by hand (a created table).
        if (!noticed.has(run.id)) {
          noticed.add(run.id);
          for (const n of run.nodes) for (const msg of n.notices ?? []) if (msg.startsWith("Created table")) useStore.getState().toast(`${n.name}: ${msg}`, "info");
        }
        // Next run time and last status in the list.
        clearTimeout(refreshTimer);
        refreshTimer = setTimeout(() => void get().refresh().catch(() => {}), 200);
        if (run.trigger === "schedule" && run.status === "error") {
          const name = get().jobs.find((j) => j.id === job_id)?.name ?? "A job";
          useStore.getState().toast(`Scheduled job "${name}" failed: ${run.error ?? "see its runs"}`, "error");
        }
      }
    });
    await get().refresh();
    // Scheduled runs started at launch, before this listener existed.
    for (const j of get().jobs) if (j.last_run && get().running[j.id]) announceScheduled(get().jobs, j.last_run);
  },
  open: (id, name) => {
    const st = useStore.getState();
    const existing = st.tabs.find((t) => t.job_id === id);
    if (existing) return st.setActiveTab(existing.id);
    st.newTab({ title: name, sql: "", connection_id: null, job_id: id });
  },
  create: async () => {
    const st = useStore.getState();
    try {
      // Up-to-date names of other jobs' steps.
      await get().refresh().catch(() => {});
      const stepName = await freeStepName("step");
      let n = 1;
      while (get().jobs.some((j) => j.name === `Job ${n}`)) n++;
      const job: Job = {
        id: "",
        name: `Job ${n}`,
        nodes: [
          {
            id: uid(),
            name: stepName,
            kind: "query",
            connection_id: st.tabs.find((t) => t.id === st.activeTabId)?.connection_id ?? st.connections[0]?.id ?? null,
            sql: "",
            x: 80,
            y: 60,
            load_mode: "append",
          },
        ],
        edges: [],
        schedule: { enabled: false, mode: "interval", minutes: 60, at: "08:00", weekdays: [] },
        created_at: 0,
        updated_at: 0,
      };
      const saved = await api.saveJob(job);
      await get().refresh();
      get().open(saved.id, saved.name);
    } catch (e) {
      st.toast(toError(e).message, "error");
    }
  },
  createFromNotebook: async (nb) => {
    const st = useStore.getState();
    const cells = nb.cells.filter((c) => c.kind === "sql" && c.source.trim());
    if (!cells.length) return st.toast("This notebook has no SQL cells", "info");
    try {
      await get().refresh().catch(() => {});
      const names: string[] = [];
      for (const [i, c] of cells.entries()) {
        // Free elsewhere and among the steps named so far.
        names.push(await freeStepName(cellStepBase(c, nb.name, i + 1), "", names));
      }
      const results = st.connections.find((c) => c.config.options?.databrain_results === "1")?.id ?? null;
      const { nodes, edges } = stepsFromCells(cells, names, nb.connection_id ?? null, results);
      let jobName = nb.name;
      for (let n = 2; get().jobs.some((j) => j.name === jobName); n++) jobName = `${nb.name} ${n}`;
      const saved = await api.saveJob({
        id: "",
        name: jobName,
        nodes,
        edges,
        schedule: { enabled: false, mode: "interval", minutes: 60, at: "08:00", weekdays: [] },
        created_at: 0,
        updated_at: 0,
      });
      await get().refresh();
      get().open(saved.id, saved.name);
      const renamed = cells.filter((c, i) => c.output_name?.trim() && c.output_name.trim().toLowerCase() !== names[i].toLowerCase()).length;
      const params = cells.some((c) => /(^|[^:\w]):[A-Za-z_]\w*/.test(c.source));
      const notes = [
        renamed ? `${renamed} output name(s) are used by the notebook, so the steps got new names (their SQL was updated)` : "",
        params ? ":name parameters are not filled in for job steps" : "",
      ].filter(Boolean);
      st.toast(`Job "${saved.name}" created with ${nodes.length} steps${notes.length ? `. ${notes.join("; ")}.` : ""}`, "info");
    } catch (e) {
      st.toast(toError(e).message, "error");
    }
  },
  remove: (j) =>
    useStore.getState().askConfirm({
      title: `Delete job "${j.name}"?`,
      reasons: ["Its steps, schedule and run history are deleted. Tables it loaded are kept."],
      confirmLabel: "Delete",
      onConfirm: async () => {
        const st = useStore.getState();
        try {
          st.tabs.filter((t) => t.job_id === j.id).forEach((t) => st.closeTab(t.id));
          await api.deleteJob(j.id);
          await get().refresh();
        } catch (e) {
          st.toast(toError(e).message, "error");
        }
      },
    }),
  run: async (id, nodes) => {
    try {
      const run = await api.runJob(id, nodes);
      set((s) => ({ runs: { ...s.runs, [id]: run }, running: { ...s.running, [id]: true } }));
    } catch (e) {
      useStore.getState().toast(toError(e).message, "error");
    }
  },
  cancel: async (id) => {
    await api.cancelJob(id).catch(() => false);
  },
}));
