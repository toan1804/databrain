// Jobs list, running jobs and their live runs (from "job-run" events).
import { create } from "zustand";
import { api, isTauri, onJobRun, onJobRunLog, toError } from "./lib/api";
import type { RunParams, Job, JobRun, JobSummary, RunLogEntry } from "./lib/types";
import { useStore } from "./store";
import { uid } from "./lib/util";
import { cellStepBase, stepsFromCells, uniqueName, type CellSource } from "./lib/jobGraph";
import { runLine } from "./components/job/shared";

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
  /** Runs seen running in this window (the activity bar follows them; their end is told). */
  seen: Record<number, true>;
  /**
   * Work the user hid from the activity bar (×) while it ran: `run:<id>`,
   * or `bg:<job id>` for the background runner's. Per run: new work shows
   * in the bar without bringing these back, and a hidden run that fails
   * shows its failure there (outcomes go by `barDismissed`).
   */
  barHidden: Record<string, true>;
  /** Finished runs whose outcome left the bar (dismissed, or shown a while). */
  barDismissed: Record<number, true>;
  /** Hide the work running now from the bar (×). */
  hideBar: (keys: string[]) => void;
  /** Show everything hidden again (the Jobs icon, the command palette). */
  showBar: () => void;
  /** A finished run's outcome leaves the bar. */
  dismissRun: (id: number) => void;
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
/** Finished runs whose notices were shown. */
const noticed = new Set<number>();
/** Finished runs whose end was told on the desktop. */
const told = new Set<number>();

/** Notification text past this is cut (a server's error can be pages long). */
export const NOTIFY_MAX = 200;

/**
 * Tell on the desktop that a run ended, when the window isn't focused (the
 * user went to another app during a long run). The jobs tray says it in the
 * window.
 */
export function notifyIfAway(jobs: JobSummary[], run: JobRun, focused: boolean) {
  if (focused) return;
  const name = jobs.find((j) => j.id === run.job_id)?.name ?? "A job";
  const how = run.status === "success" ? "finished" : run.status === "cancelled" ? "stopped" : "failed";
  const line = runLine(run, Date.now());
  // By code points: a cut through an emoji's surrogate pair would be text
  // the app refuses (the notification lost without a sign).
  const chars = Array.from(line);
  const body = chars.length > NOTIFY_MAX ? `${chars.slice(0, NOTIFY_MAX - 1).join("")}…` : line;
  void api.notifyJobRun(`Job "${name}" ${how}`, body, run.status === "error").catch(() => {});
}

/**
 * The toast for a failed scheduled or "run after" run, when the jobs tray
 * has no card for it (it never was seen running here). A run seen running
 * has its card (or its tab shown), which says it already.
 */
export function failureToast(jobs: JobSummary[], run: JobRun, seen: boolean): string | null {
  if (seen || run.status !== "error" || !(run.trigger === "schedule" || run.trigger.startsWith("after:"))) return null;
  const name = jobs.find((j) => j.id === run.job_id)?.name ?? "A job";
  const what = run.trigger === "schedule" ? `Scheduled job "${name}" failed` : `"${name}" (run after "${run.trigger.slice(6)}" succeeded) failed`;
  return `${what}: ${run.error ?? "see its runs"}`;
}
let refreshTimer: ReturnType<typeof setTimeout> | undefined;

/** Live log lines kept per run (the app keeps the same number). */
export const LIVE_LOG_MAX = 5000;
/** Runs whose live log is kept; a finished run's log is read from the app. */
export const LIVE_LOG_RUNS = 10;

/** `logs` with `entries` added to run `runId` (lines already there skipped),
 * keeping each run's latest {@link LIVE_LOG_MAX} lines and the latest
 * {@link LIVE_LOG_RUNS} runs. Returns `logs` itself when nothing is new. */
export function appendRunLog(logs: Record<number, RunLogEntry[]>, runId: number, entries: RunLogEntry[]): Record<number, RunLogEntry[]> {
  const cur = logs[runId] ?? [];
  const last = cur.length ? cur[cur.length - 1].seq : 0;
  const fresh = entries.filter((e) => e.seq > last);
  if (!fresh.length) return logs;
  const merged = cur.concat(fresh);
  const next = { ...logs, [runId]: merged.length > LIVE_LOG_MAX ? merged.slice(merged.length - LIVE_LOG_MAX) : merged };
  const ids = Object.keys(next).map(Number);
  if (ids.length > LIVE_LOG_RUNS) {
    // Run ids grow: the smallest are the oldest runs.
    for (const id of ids.sort((a, b) => a - b).slice(0, ids.length - LIVE_LOG_RUNS)) if (id !== runId) delete next[id];
  }
  return next;
}

export const useJobs = create<JobsState>((set, get) => ({
  jobs: [],
  runs: {},
  running: {},
  elsewhere: {},
  logs: {},
  seen: {},
  barHidden: {},
  barDismissed: {},
  hideBar: (keys) => set((s) => ({ barHidden: { ...s.barHidden, ...Object.fromEntries(keys.map((k) => [k, true as const])) } })),
  showBar: () => set((s) => (Object.keys(s.barHidden).length ? { barHidden: {} } : {})),
  dismissRun: (id) => set((s) => (s.barDismissed[id] ? {} : { barDismissed: { ...s.barDismissed, [id]: true as const } })),
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
        const logs = appendRunLog(s.logs, run_id, entries);
        return logs === s.logs ? {} : { logs };
      }),
    );
    await onJobRun(runEvent);
    // Runs of the background runner (another process) send no events: look
    // again now and then. Set up before the first look, which may fail (and
    // is then tried again by these).
    if (typeof window !== "undefined") {
      setInterval(() => void get().refresh().catch(() => {}), 30_000);
      window.addEventListener("focus", () => void get().refresh().catch(() => {}));
    }
    await get().refresh();
    // Runs started at launch (a scheduled one), before this listener
    // existed: the jobs tray follows them too.
    set((s) => {
      const started = get().jobs.flatMap((j) => (j.last_run?.status === "running" && s.running[j.id] && !s.runs[j.id] ? [j.last_run] : []));
      if (!started.length) return {};
      return {
        runs: { ...s.runs, ...Object.fromEntries(started.map((r) => [r.job_id, r])) },
        seen: { ...s.seen, ...Object.fromEntries(started.map((r) => [r.id, true as const])) },
      };
    });
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
    const toast = useStore.getState().toast;
    // The background runner's run is in another process: its steps can't be stopped from here.
    if (get().elsewhere[id] && !get().running[id]) return toast("This run belongs to the background runner: its steps can't be stopped from here", "info");
    try {
      if (!(await api.cancelJobStep(id, nodeId))) toast("The step wasn't stopped: it had already ended", "info");
    } catch (e) {
      toast(`The step couldn't be stopped: ${toError(e).message}`, "error");
    }
  },
}));

/**
 * A "job-run" event: the run's latest state. Keeps the runs seen running
 * (the activity bar follows them), brings the bar back for new work and
 * for a hidden run that fails, tells on the desktop when one ends while
 * the window isn't focused, and toasts what the bar doesn't show.
 */
export function runEvent({ job_id, run }: { job_id: string; run: JobRun }) {
  const key = `run:${run.id}`;
  const before = useJobs.getState();
  const isNew = run.status === "running" && !before.seen[run.id];
  const wasHidden = !!before.barHidden[key];
  // Its job's tab is shown: the tab tells how it ended, the bar needn't.
  const st = useStore.getState();
  const inView = st.tabs.find((t) => t.id === st.activeTabId)?.job_id === job_id;
  useJobs.setState((s) => ({
    runs: { ...s.runs, [job_id]: run },
    running: { ...s.running, [job_id]: run.status === "running" },
    ...(isNew ? { seen: { ...s.seen, [run.id]: true as const } } : {}),
    // Ended: its key goes (the outcome is kept or dismissed below).
    ...(run.status !== "running" && wasHidden ? { barHidden: Object.fromEntries(Object.entries(s.barHidden).filter(([k]) => k !== key)) } : {}),
    ...(run.status !== "running" && (inView || (wasHidden && run.status !== "error")) ? { barDismissed: { ...s.barDismissed, [run.id]: true as const } } : {}),
  }));
  const seen = !!useJobs.getState().seen[run.id];
  // Hidden and done without a failure: a short toast, not the bar again.
  if (wasHidden && !inView && (run.status === "success" || run.status === "cancelled")) {
    const name = useJobs.getState().jobs.find((j) => j.id === job_id)?.name ?? "A job";
    useStore.getState().toast(`Job "${name}": ${runLine(run, Date.now())}`, run.status === "success" ? "success" : "info");
  }
  if (run.status !== "running" && seen && !told.has(run.id)) {
    told.add(run.id);
    notifyIfAway(useJobs.getState().jobs, run, typeof document === "undefined" || document.hasFocus());
  }
  if (run.status !== "running") {
    // Tell what a step did that the user didn't set up by hand (a created table).
    if (!noticed.has(run.id)) {
      noticed.add(run.id);
      for (const n of run.nodes) for (const msg of n.notices ?? []) if (msg.startsWith("Created table")) useStore.getState().toast(`${n.name}: ${msg}`, "info");
    }
    // Next run time and last status in the list.
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(() => void useJobs.getState().refresh().catch(() => {}), 200);
    const failed = failureToast(useJobs.getState().jobs, run, seen);
    if (failed) useStore.getState().toast(failed, "error");
  }
}
