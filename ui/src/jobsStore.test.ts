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
const { useJobs } = await import("./jobsStore");

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
