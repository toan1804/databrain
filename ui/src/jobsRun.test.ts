import { describe, expect, it, vi } from "vitest";
import type { JobRun } from "./lib/types";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

let onRun: ((e: { job_id: string; run: JobRun }) => void) | null = null;
const run = (status: string, nodes: JobRun["nodes"] = []): JobRun => ({ id: 7, job_id: "j", trigger: "manual", started_at: 0, status, nodes }) as JobRun;

vi.mock("./lib/api", () => ({
  isTauri: () => true,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobRun: async (h: typeof onRun) => {
    onRun = h;
    return () => {};
  },
  onJobRunLog: async () => () => {},
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  api: {
    listJobs: async () => ({ jobs: [], running: [] }),
    runJob: async () => {
      // A step that fails at once: the run ends before runJob replies.
      onRun?.({ job_id: "j", run: run("error", [{ node_id: "n1", name: "a", status: "error", finished_at: 1, duration_ms: 1, error: "can't connect" } as never]) });
      return run("running");
    },
  },
}));

describe("job runs", () => {
  it("keeps the finished run when its events beat runJob's reply", async () => {
    const { useJobs } = await import("./jobsStore");
    await useJobs.getState().init();
    await useJobs.getState().run("j", ["n1"]);
    expect(useJobs.getState().runs.j.status).toBe("error");
    expect(useJobs.getState().running.j).toBe(false);
  });
});
