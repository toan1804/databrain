import { describe, expect, it, vi, beforeEach } from "vitest";
import { act, render, screen } from "@testing-library/react";
import type { JobRun } from "../lib/types";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });
vi.mock("../lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  api: { cancelJob: vi.fn(async () => true) },
}));

const { useJobs } = await import("../jobsStore");
const { useStore } = await import("../store");
const { JobsTray, DONE_CARD_MS } = await import("./JobsTray");
const { runLine } = await import("./job/shared");

const node = (name: string, status: string, extra = {}) => ({ node_id: name, name, status, finished_at: 0, duration_ms: 0, ...extra }) as JobRun["nodes"][number];
const run = (status: JobRun["status"], nodes: JobRun["nodes"], extra: Partial<JobRun> = {}): JobRun => ({ id: 1, job_id: "j", trigger: "manual", started_at: 0, status, nodes, ...extra });
const loading = node("load_orders", "running", { progress: { phase: "Inserting rows", done: 250_000, total: 1_000_000, elapsed_ms: 10_000 } });

function setRun(r: JobRun) {
  act(() => useJobs.setState({ runs: { j: r }, running: { j: r.status === "running" } }));
}

beforeEach(() => {
  useJobs.setState({ jobs: [{ id: "j", name: "Nightly sales" } as never], runs: {}, running: {}, elsewhere: {} });
  useStore.setState({ tabs: [{ id: "t1", title: "Query", sql: "", connection_id: null } as never], activeTabId: "t1" });
});

describe("a running job's line", () => {
  it("names the running step, how far it got and the time taken", () => {
    const r = run("running", [node("read", "success"), loading, node("after", "pending")]);
    expect(runLine(r, 65_000)).toBe("1 of 3 steps done · load_orders: Inserting rows: 250,000 / 1,000,000 rows · 25% · 25,000 rows/s · about 30s left · 1m 5s");
    expect(runLine(run("running", [node("a", "running"), node("b", "running")]), 3_000)).toBe("0 of 2 steps done · 2 steps running · 3.00 s");
  });

  it("says how the run ended", () => {
    expect(runLine(run("success", [node("a", "success")], { finished_at: 90_000 }), 0)).toBe("Finished in 1m 30s · 1 step");
    expect(runLine(run("error", [node("a", "success"), node("b", "error", { error: "permission denied" })], { finished_at: 1 }), 0)).toBe("Failed at b: permission denied");
    expect(runLine(run("cancelled", [], { finished_at: 4_000 }), 0)).toBe("Stopped after 4.00 s");
  });
});

describe("the jobs tray", () => {
  it("shows a job running while the user is in another tab, then how it ended", () => {
    render(<JobsTray />);
    setRun(run("running", [loading]));
    expect(screen.getByRole("status", { name: "Job Nightly sales" }).textContent).toContain("load_orders: Inserting rows");
    expect(screen.getByRole("button", { name: /Stop/ })).toBeTruthy();
    setRun(run("error", [node("load_orders", "error", { error: "disk full" })], { finished_at: 5 }));
    expect(screen.getByRole("alert", { name: "Job Nightly sales" }).textContent).toContain("Failed at load_orders: disk full");
    act(() => screen.getByRole("button", { name: "Dismiss" }).click());
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("has no card while the job's own tab is shown, nor for a run finished before the app saw it run", () => {
    useStore.setState({ tabs: [{ id: "t2", title: "Nightly sales", sql: "", connection_id: null, job_id: "j" } as never], activeTabId: "t2" });
    render(<JobsTray />);
    setRun(run("running", [loading]));
    expect(screen.queryByRole("status")).toBeNull();
    // The user goes to an editor: the card shows.
    act(() => useStore.setState({ tabs: [...useStore.getState().tabs, { id: "t1", title: "Query", sql: "", connection_id: null } as never], activeTabId: "t1" }));
    expect(screen.getByRole("status", { name: "Job Nightly sales" })).toBeTruthy();

    setRun(run("success", [node("a", "success")], { id: 9, finished_at: 5 }));
    expect(screen.queryByRole("status", { name: "Job Nightly sales" })).toBeNull();
  });

  it("lets a successful run's card go by itself", () => {
    vi.useFakeTimers();
    try {
      render(<JobsTray />);
      setRun(run("running", [loading]));
      setRun(run("success", [node("load_orders", "success")], { finished_at: 60_000 }));
      expect(screen.getByRole("status", { name: "Job Nightly sales" }).textContent).toContain("Finished in 1m 0s");
      act(() => vi.advanceTimersByTime(DONE_CARD_MS + 10));
      expect(screen.queryByRole("status", { name: "Job Nightly sales" })).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("the desktop notification", () => {
  it("is sent when a run ends while the window isn't focused, not while it is", async () => {
    const { api } = await import("../lib/api");
    const notify = vi.fn(async () => {});
    (api as unknown as { notifyJobRun: typeof notify }).notifyJobRun = notify;
    const { notifyIfAway } = await import("../jobsStore");
    const jobs = [{ id: "j", name: "Nightly sales" } as never];
    notifyIfAway(jobs, run("error", [node("load", "error", { error: "disk full" })], { finished_at: 1 }), true);
    expect(notify).not.toHaveBeenCalled();
    notifyIfAway(jobs, run("error", [node("load", "error", { error: "disk full" })], { finished_at: 1 }), false);
    notifyIfAway(jobs, run("success", [node("load", "success")], { finished_at: 120_000 }), false);
    expect(notify.mock.calls).toEqual([
      ['Job "Nightly sales" failed', "Failed at load: disk full", true],
      ['Job "Nightly sales" finished', "Finished in 2m 0s · 1 step", false],
    ]);
  });
});
