import { beforeEach, describe, expect, it, vi } from "vitest";

const saved: unknown[] = [];
vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onJobRun: async () => () => {},
  onAuthEvent: async () => () => {},
  onOracleAgent: async () => () => {},
  api: new Proxy(
    {},
    {
      get: (_t, name) => async (arg: unknown) => {
        if (name === "saveJob") {
          saved.push(arg);
          return { ...(arg as object), id: "job1" };
        }
        if (name === "runJob") return { id: 1, job_id: "job1", trigger: "manual", started_at: 0, status: "running", nodes: [] };
        if (name === "listJobs") return { jobs: [], running: [] };
      },
    },
  ),
}));

const { useStore, isQueryTab } = await import("./store");
const { useJobs, appendRunLog, LIVE_LOG_MAX, LIVE_LOG_RUNS } = await import("./jobsStore");

beforeEach(() => {
  saved.length = 0;
  useStore.setState({ tabs: [{ id: "q", title: "Sales", sql: "select 1", connection_id: "pg" }], activeTabId: "q", runs: {} });
});

describe("jobs store", () => {
  it("creates a job with one step on the active tab's connection and opens it in a tab (reused)", async () => {
    await useJobs.getState().create();
    const job = saved[0] as { name: string; nodes: { connection_id: string; kind: string }[] };
    expect(job.name).toBe("Job 1");
    expect(job.nodes).toHaveLength(1);
    expect(job.nodes[0]).toMatchObject({ connection_id: "pg", kind: "query" });
    const st = useStore.getState();
    const tab = st.tabs.find((t) => t.id === st.activeTabId)!;
    expect(tab.job_id).toBe("job1");
    expect(isQueryTab(tab)).toBe(false);
    useJobs.getState().open("job1", "Job 1");
    expect(useStore.getState().tabs.filter((t) => t.job_id)).toHaveLength(1);
  });

  it("tracks a started run as running", async () => {
    await useJobs.getState().run("job1");
    expect(useJobs.getState().running.job1).toBe(true);
    expect(useJobs.getState().runs.job1.status).toBe("running");
  });

  it("a new job's first step gets a name no other job uses", async () => {
    useJobs.setState({ jobs: [{ id: "j1", name: "Job 1", node_count: 2, step_names: ["step", "Step_2"], schedule: { enabled: false, mode: "interval", minutes: 60, at: "08:00", weekdays: [] }, updated_at: 0 }] });
    await useJobs.getState().create();
    const job = saved[0] as { name: string; nodes: { name: string }[] };
    expect(job.name).toBe("Job 2");
    expect(job.nodes[0].name).toBe("step_3");
  });

  it("creates a job from a notebook's SQL cells", async () => {
    useJobs.setState({ jobs: [] });
    await useJobs.getState().createFromNotebook({
      name: "Revenue",
      connection_id: "pg",
      cells: [
        { kind: "markdown", source: "# notes" },
        { kind: "sql", source: "select 1", output_name: "base" },
        { kind: "sql", source: "select * from results.base" },
      ],
    });
    const job = saved[0] as { name: string; nodes: { name: string; connection_id: string }[]; edges: unknown[] };
    expect(job.name).toBe("Revenue");
    expect(job.nodes.map((n) => [n.name, n.connection_id])).toEqual([["base", "pg"], ["revenue_2", "pg"]]);
    expect(job.edges).toHaveLength(1);
    expect(useStore.getState().tabs.find((t) => t.id === useStore.getState().activeTabId)!.job_id).toBe("job1");
  });
});

describe("scheduled runs", () => {
  it("announces a scheduled run once, and not manual or finished ones", async () => {
    const { announceScheduled } = await import("./jobsStore");
    const toasts: string[] = [];
    const orig = useStore.getState().toast;
    useStore.setState({ toast: (m: string) => void toasts.push(m) });
    const jobs = [{ id: "j1", name: "Nightly", node_count: 1, step_names: [], schedule: { enabled: true, mode: "interval" as const, minutes: 60, at: "08:00", weekdays: [] }, updated_at: 0 }];
    const run = { id: 7, job_id: "j1", trigger: "schedule" as const, started_at: 0, status: "running" as const, nodes: [] };
    announceScheduled(jobs, run);
    announceScheduled(jobs, { ...run, nodes: [] });
    announceScheduled(jobs, { ...run, id: 8, trigger: "manual" });
    announceScheduled(jobs, { ...run, id: 9, status: "success" });
    expect(toasts).toEqual(['Starting scheduled job "Nightly"…']);
    useStore.setState({ toast: orig });
  });
});

describe("live run logs", () => {
  const entry = (seq: number) => ({ seq, at: 0, level: "info" as const, message: `line ${seq}` });
  it("skips lines already there and returns the same object when nothing is new", () => {
    const a = appendRunLog({}, 1, [entry(1), entry(2)]);
    const b = appendRunLog(a, 1, [entry(2), entry(3)]);
    expect(b[1].map((e) => e.seq)).toEqual([1, 2, 3]);
    expect(appendRunLog(b, 1, [entry(3)])).toBe(b);
  });
  it("keeps each run's latest lines and the latest runs", () => {
    let logs = appendRunLog({}, 1, Array.from({ length: LIVE_LOG_MAX + 10 }, (_, i) => entry(i + 1)));
    expect(logs[1]).toHaveLength(LIVE_LOG_MAX);
    expect(logs[1][0].seq).toBe(11);
    for (let id = 2; id <= LIVE_LOG_RUNS + 3; id++) logs = appendRunLog(logs, id, [entry(1)]);
    const ids = Object.keys(logs).map(Number);
    expect(ids).toHaveLength(LIVE_LOG_RUNS);
    expect(Math.min(...ids)).toBe(4);
  });
});
