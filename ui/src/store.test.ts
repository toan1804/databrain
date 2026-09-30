import { beforeEach, describe, expect, it, vi } from "vitest";
import type { JobEvent, RunRequest } from "./lib/types";

// Fake backend: run_query emits the job's events *before* resolving, like a
// warm session finishing a fast statement before the IPC response arrives.
let jobSeq = 0;
let emitBeforeResolve = true;
const cancelResult = { value: false };
let handler: ((e: JobEvent) => void) | null = null;

vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  api: {
    runQuery: async (req: RunRequest) => {
      const job_id = `job-${++jobSeq}`;
      const plan = { index: 0, sql: req.sql, start: 0, end: req.sql.length, classification: { kind: "read", missing_where: false, keyword: "SELECT" } };
      if (emitBeforeResolve) {
        const t = req.tab_id;
        handler?.({ type: "statement_started", job_id, tab_id: t, index: 0, sql: req.sql });
        handler?.({ type: "statement_finished", job_id, tab_id: t, index: 0, result: null, rows_affected: 0, duration_ms: 1, notices: [] });
        handler?.({ type: "job_finished", job_id, tab_id: t, status: "success", duration_ms: 1 });
      }
      return { status: "started", job_id, statements: [plan] };
    },
    cancelQuery: async () => cancelResult.value,
  },
}));

const { useStore, _earlyEventCount } = await import("./store");

beforeEach(() => {
  handler = (e) => useStore.getState().handleJobEvent(e);
  emitBeforeResolve = true;
  useStore.setState({ runs: {}, connections: [], backendAvailable: true });
});

describe("job event ordering", () => {
  it("does not get stuck when the job finishes before run_query resolves", async () => {
    const key = "nb:n1:c1";
    for (let i = 0; i < 3; i++) {
      // Repeated runs (the "Run all twice" case).
      expect(await useStore.getState().runSql(key, "conn", "select 1", 0, "nb:n1")).toBe(true);
      const run = useStore.getState().runs[key];
      expect(run.running).toBe(false);
      expect(run.finishedStatus).toBe("success");
      expect(run.statements[0].status).toBe("done");
    }
    expect(_earlyEventCount()).toBe(0);
  });

  it("Stop clears a run whose job the backend no longer knows", async () => {
    emitBeforeResolve = false; // events lost entirely
    const key = "tab-1";
    await useStore.getState().runSql(key, "conn", "select 1");
    expect(useStore.getState().runs[key].running).toBe(true);
    cancelResult.value = false;
    await useStore.getState().cancelTab(key);
    const run = useStore.getState().runs[key];
    expect(run.running).toBe(false);
    expect(run.finishedStatus).toBe("cancelled");
  });

  it("Stop leaves a live job to the backend's cancel event", async () => {
    emitBeforeResolve = false;
    const key = "tab-2";
    await useStore.getState().runSql(key, "conn", "select 1");
    cancelResult.value = true;
    await useStore.getState().cancelTab(key);
    expect(useStore.getState().runs[key].running).toBe(true);
  });
});
