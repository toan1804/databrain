// Jobs list, running jobs and their live runs (from "job-run" events).
import { create } from "zustand";
import { api, isTauri, onJobRun, onJobRunLog, toError } from "./lib/api";
import type { RunParams, Job, JobRun, JobSummary, RunLogEntry } from "./lib/types";
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
  /** Running in the background runner (another process): shown, not stoppable here. */
  elsewhere: Record<string, boolean>;
  /** Log lines of runs seen live in this session, by run id. */
  logs: Record<number, RunLogEntry[]>;
  refresh: () => Promise<void>;
  init: () => Promise<void>;
  open: (id: string, name: string) => void;
  create: () => Promise<void>;
  /** A job with one step per SQL cell of a notebook, top to bottom. */
  /** `paramsKey`: the notebook's parameter boxes, whose values the job takes. */
  createFromNotebook: (nb: { name: string; connection_id?: string | null; cells: (CellSource & { kind: string })[] }, paramsKey?: string) => Promise<void>;
  remove: (j: { id: string; name: string }) => void;
  run: (id: string, nodes?: string[], params?: RunParams) => Promise<void>;
  /** Run again the steps that did not succeed last time (after a fix). */
  resume: (id: string) => Promise<void>;
  cancel: (id: string) => Promise<void>;
  /** Stop one running step (the others go on). */
  cancelStep: (id: string, nodeId: string) => Promise<void>;
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
  elsewhere: {},
  logs: {},
  refresh: async () => {
    if (!isTauri()) return;
    const { jobs, running, elsewhere } = await api.listJobs();
    // A list read before a run started (e.g. the save right before "Run job")
    // must not hide that run: runs whose events say "running" stay running.
    // But a run the app saved as finished (an event that never arrived) must
    // not stay "running" here, with its steps "Waiting for upstream steps".
    set((s) => {
      const live = new Set([...running, ...(elsewhere ?? [])]);
      const runs = { ...s.runs };
      for (const j of jobs) {
        const cur = runs[j.id];
        const saved = j.last_run;
        if (cur?.status === "running" && saved && saved.id >= cur.id && saved.status !== "running" && !live.has(j.id)) runs[j.id] = saved;
      }
      return {
        jobs,
        runs,
        elsewhere: Object.fromEntries((elsewhere ?? []).map((id) => [id, true])),
        running: {
          ...Object.fromEntries(Object.entries(runs).filter(([, r]) => r.status === "running").map(([id]) => [id, true])),
          ...Object.fromEntries(running.map((id) => [id, true])),
        },
      };
    });
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
    // Runs of the background runner (another process) send no events: look again now and then.
    if (typeof window !== "undefined") {
      setInterval(() => void get().refresh().catch(() => {}), 30_000);
      window.addEventListener("focus", () => void get().refresh().catch(() => {}));
    }
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
  createFromNotebook: async (nb, paramsKey) => {
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
        // The notebook's current :name values become the job's own.
        params: Object.fromEntries(Object.entries(paramsKey ? (st.params[paramsKey] ?? {}) : {}).filter(([, v]) => v.value.trim())),
        created_at: 0,
        updated_at: 0,
      });
      await get().refresh();
      get().open(saved.id, saved.name);
      const renamed = cells.filter((c, i) => c.output_name?.trim() && c.output_name.trim().toLowerCase() !== names[i].toLowerCase()).length;
      const params = cells.some((c) => /(^|[^:\w]):[A-Za-z_]\w*/.test(c.source)) && !Object.keys(saved.params ?? {}).length;
      const notes = [
        renamed ? `${renamed} output name(s) are used by the notebook, so the steps got new names (their SQL was updated)` : "",
        params ? ":name parameters have no values yet: set them in the job's Parameters" : "",
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
  run: async (id, nodes, params) => {
    try {
      const run = await api.runJob(id, nodes, params);
      // The run's events can arrive before this reply (a step that fails at
      // once, e.g. on a connection that can't connect): keep the newer state
      // instead of putting back the "just started" run, which stayed
      // "Waiting for upstream steps" forever.
      set((s) => (s.runs[id] && s.runs[id].id >= run.id ? {} : { runs: { ...s.runs, [id]: run }, running: { ...s.running, [id]: true } }));
    } catch (e) {
      useStore.getState().toast(toError(e).message, "error");
    }
  },
  resume: async (id) => {
    try {
      const run = await api.resumeJob(id);
      set((s) => (s.runs[id] && s.runs[id].id >= run.id ? {} : { runs: { ...s.runs, [id]: run }, running: { ...s.running, [id]: true } }));
    } catch (e) {
      useStore.getState().toast(toError(e).message, "error");
    }
  },
  cancel: async (id) => {
    await api.cancelJob(id).catch(() => false);
  },
  cancelStep: async (id, nodeId) => {
    await api.cancelJobStep(id, nodeId).catch(() => false);
  },
}));
