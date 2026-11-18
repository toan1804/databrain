import { describe, expect, it, vi, beforeEach } from "vitest";
import { act, render, screen } from "@testing-library/react";
import type { JobRun } from "../lib/types";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });
vi.mock("../lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  api: { cancelJob: vi.fn(async () => true), notifyJobRun: vi.fn(async () => {}) },
}));

const { useJobs, runEvent } = await import("../jobsStore");
/** A "job-run" event, through the store's own handler. */
const event = (r: JobRun) => runEvent({ job_id: r.job_id, run: r });
const { useStore } = await import("../store");
const { ActivityBar, DONE_MS } = await import("./ActivityBar");
const { runLine } = await import("./job/shared");

const node = (name: string, status: string, extra = {}) => ({ node_id: name, name, status, finished_at: 0, duration_ms: 0, ...extra }) as JobRun["nodes"][number];
const run = (status: JobRun["status"], nodes: JobRun["nodes"], extra: Partial<JobRun> = {}): JobRun => ({ id: 1, job_id: "j", trigger: "manual", started_at: 0, status, nodes, ...extra });
const loading = node("load_orders", "running", { progress: { phase: "Inserting rows", done: 250_000, total: 1_000_000, elapsed_ms: 10_000 } });

function setRun(r: JobRun) {
  act(() => event(r));
}

beforeEach(() => {
  useJobs.setState({ jobs: [{ id: "j", name: "Nightly sales" } as never, { id: "k", name: "Hourly" } as never], runs: {}, running: {}, elsewhere: {}, seen: {}, barHidden: {}, barDismissed: {} });
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

const bar = () => screen.queryByRole("region", { name: "Background activity" });

describe("the activity bar", () => {
  it("is there only while work runs: the running step, then how the run ended", () => {
    const { container } = render(<ActivityBar />);
    expect(bar()).toBeNull();
    expect((container.firstElementChild as HTMLElement).style.height).toBe("0px");
    setRun(run("running", [loading]));
    expect(bar()!.textContent).toContain("Nightly sales · load_orders: Inserting rows: 250,000 / 1,000,000 rows");
    expect((container.firstElementChild as HTMLElement).style.height).toBe("24px");
    // The details: Open and Stop.
    act(() => screen.getByRole("button", { name: /Nightly sales/ }).click());
    expect(screen.getByRole("dialog", { name: "Running jobs" }).textContent).toContain("Stop");
    setRun(run("error", [node("load_orders", "error", { error: "disk full" })], { finished_at: 5 }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByRole("alert", { name: "Job Nightly sales" }).textContent).toContain("Failed at load_orders: disk full");
    act(() => screen.getByRole("button", { name: "Dismiss Nightly sales" }).click());
    expect(bar()).toBeNull();
  });

  it("says how many run, and shows the background runner's runs without Stop", () => {
    render(<ActivityBar />);
    setRun(run("running", [loading]));
    act(() => useJobs.setState({ elsewhere: { k: true } }));
    expect(bar()!.textContent).toContain("2 jobs running · Nightly sales 25%, Hourly");
    act(() => screen.getByRole("button", { name: /2 jobs running/ }).click());
    const rows = screen.getByRole("dialog", { name: "Running jobs" });
    expect(rows.textContent).toContain("Running in the background runner");
    expect(screen.getAllByRole("button", { name: /Stop/ })).toHaveLength(1);
  });

  it("has no outcome for a run that ended while its tab was shown, nor for one never seen running", () => {
    useStore.setState({ tabs: [{ id: "t2", title: "Nightly sales", sql: "", connection_id: null, job_id: "j" } as never], activeTabId: "t2" });
    render(<ActivityBar />);
    setRun(run("running", [loading]));
    expect(bar()).not.toBeNull();
    setRun(run("error", [node("load_orders", "error", { error: "x" })], { finished_at: 5 }));
    expect(bar()).toBeNull();
    setRun(run("success", [node("a", "success")], { id: 9, finished_at: 5 }));
    expect(bar()).toBeNull();
  });

  it("shows a run that started and failed between two renders", () => {
    render(<ActivityBar />);
    act(() => {
      event(run("running", [loading]));
      event(run("error", [node("load_orders", "error", { error: "disk full" })], { finished_at: 5 }));
    });
    expect(screen.getByRole("alert", { name: "Job Nightly sales" }).textContent).toContain("disk full");
  });

  it("lets each success go on its own clock, and keeps a failure", () => {
    vi.useFakeTimers();
    try {
      render(<ActivityBar />);
      setRun(run("running", [loading]));
      setRun(run("running", [loading], { id: 2, job_id: "k" }));
      setRun(run("running", [loading], { id: 3, job_id: "f" }));
      setRun(run("success", [node("load_orders", "success")], { finished_at: 1 }));
      act(() => vi.advanceTimersByTime(DONE_MS - 1_000));
      setRun(run("success", [node("load_orders", "success")], { id: 2, job_id: "k", finished_at: 1 }));
      setRun(run("error", [node("load_orders", "error", { error: "boom" })], { id: 3, job_id: "f", finished_at: 1 }));
      act(() => vi.advanceTimersByTime(1_010));
      expect(screen.queryByRole("status", { name: "Job Nightly sales" })).toBeNull();
      expect(screen.getByRole("status", { name: "Job Hourly" })).toBeTruthy();
      act(() => vi.advanceTimersByTime(DONE_MS));
      expect(screen.queryByRole("status", { name: "Job Hourly" })).toBeNull();
      expect(screen.getByRole("alert", { name: "Job Job" })).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("hides with ×: a hidden success is a toast, a hidden failure and new work bring it back", () => {
    const toasts: string[] = [];
    const orig = useStore.getState().toast;
    useStore.setState({ toast: (m: string) => void toasts.push(m) });
    try {
      render(<ActivityBar />);
      setRun(run("running", [loading]));
      setRun(run("running", [loading], { id: 2, job_id: "k" }));
      act(() => screen.getByRole("button", { name: "Hide the activity bar" }).click());
      expect(bar()).toBeNull();
      // Hidden and succeeded: a toast, the bar stays away.
      setRun(run("success", [node("a", "success")], { finished_at: 60_000 }));
      expect(bar()).toBeNull();
      expect(toasts).toEqual(['Job "Nightly sales": Finished in 1m 0s · 1 step']);
      // Hidden and failed: back, with the failure (and what still runs).
      setRun(run("error", [node("a", "error", { error: "disk full" })], { id: 2, job_id: "k", finished_at: 5 }));
      expect(screen.getByRole("alert", { name: "Job Hourly" })).toBeTruthy();
      act(() => screen.getByRole("button", { name: "Hide the activity bar" }).click());
      expect(bar()).toBeNull();
      // New work: back.
      setRun(run("running", [loading], { id: 4 }));
      expect(bar()!.textContent).toContain("Nightly sales");
      // The Jobs icon or the command palette show what was hidden.
      act(() => screen.getByRole("button", { name: "Hide the activity bar" }).click());
      act(() => useJobs.getState().showBar());
      expect(bar()).not.toBeNull();
    } finally {
      useStore.setState({ toast: orig });
    }
  });
});

describe("the desktop notification", () => {
  it("is sent when a run ends while the window isn't focused, not while it is", async () => {
    const { api } = await import("../lib/api");
    const notify = vi.mocked(api.notifyJobRun);
    notify.mockClear();
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
    // A server's long error is cut.
    notifyIfAway(jobs, run("error", [node("load", "error", { error: "x".repeat(5_000) })], { finished_at: 1 }), false);
    const body = (notify.mock.calls[2] as unknown as string[])[1];
    expect(body.length).toBe(200);
    expect(body.endsWith("…")).toBe(true);
    // An emoji where the cut falls stays whole (no half surrogate pair).
    const line = runLine(run("error", [node("load", "error", { error: "x".repeat(176) + "🔥".repeat(30) })], { finished_at: 1 }), 0);
    expect(Array.from(line)[198]).toBe("🔥");
    notifyIfAway(jobs, run("error", [node("load", "error", { error: "x".repeat(176) + "🔥".repeat(30) })], { finished_at: 1 }), false);
    const cut = (notify.mock.calls[3] as unknown as string[])[1];
    expect(Array.from(cut)).toHaveLength(200);
    expect(cut.endsWith("🔥…")).toBe(true);
    expect(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])/.test(cut)).toBe(false);
  });
});
