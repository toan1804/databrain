import { beforeEach, describe, expect, it, vi } from "vitest";
import type { JobEvent, OutputInfo, RunRequest } from "./lib/types";

// Fake backend: run_query emits the job's events *before* resolving, like a
// warm session finishing a fast statement before the IPC response arrives.
let jobSeq = 0;
let emitBeforeResolve = true;
const cancelResult = { value: false };
let handler: ((e: JobEvent) => void) | null = null;
const out = (handle: string, tab: string, index: number, extra: Record<string, unknown> = {}) =>
  ({ handle, tab_id: tab, statement_index: index, active: true, state: "on_disk", result_id: `res-${handle}`, sql: `select ${index}`, columns: [], rows: 3, truncated: false, bytes: 10, created_at: 1, ...extra }) as unknown as OutputInfo;
let outputs: OutputInfo[] = [];
const deleted: string[] = [];
const savedNotebooks: Record<string, unknown>[] = [];

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
    deleteConnection: async (id: string) => {
      deleted.push(id);
    },
    listConnections: async () => [],
    saveNotebook: async (nb: Record<string, unknown>) => {
      savedNotebooks.push(nb);
      return { ...nb, id: "nb-new" };
    },
    listNotebooks: async () => [],
    loadOutput: async (handle: string) => ({ ...outputs.find((o) => o.handle === handle)!, state: "live" }),
  },
}));

const { useStore, _earlyEventCount, restoredRun, RESTORED_NOTICE } = await import("./store");

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

describe("restoring tab results after a restart", () => {
  it("builds a finished run from the tab's saved outputs", () => {
    const run = restoredRun("t1", [out("r3", "t1", 1), out("r2", "t1", 0), out("r9", "t2", 0), out("r1", "t1", 0, { active: false })])!;
    expect(run.statements.map((s) => s.output?.handle)).toEqual(["r2", "r3"]);
    expect(run.statements[1].result).toMatchObject({ id: "res-r3", total_rows: 3, complete: true });
    expect(run.statements[0].notices).toEqual([RESTORED_NOTICE]);
    expect(run.running).toBe(false);
    expect(run.activeIndex).toBe(1);
    expect(restoredRun("t3", [out("r1", "t1", 0)])).toBeNull();
  });

  it("loads saved outputs and does not overwrite a newer run", async () => {
    outputs = [out("r1", "a", 0), out("r2", "b", 0)];
    useStore.setState({
      tabs: [{ id: "a", title: "A", sql: "" }, { id: "b", title: "B", sql: "" }],
      activeTabId: "b",
      outputs,
      runs: { a: { jobId: "j", running: true, startedAt: 0, statements: [], activeIndex: null } },
    });
    await useStore.getState().restoreTabOutputs();
    const runs = useStore.getState().runs;
    expect(runs.a.jobId).toBe("j"); // kept
    expect(runs.b.statements[0].output?.state).toBe("live"); // loaded from disk
  });
});

describe("deleting a connection", () => {
  const conn = { id: "c1", name: "Prod PG", env: "prod", connected: true } as never;
  it("asks first and does nothing when cancelled", async () => {
    useStore.setState({ connections: [conn], tabs: [{ id: "t", title: "T", sql: "", connection_id: "c1" }] });
    const p = useStore.getState().deleteConnection("c1");
    const ask = useStore.getState().confirm!;
    expect(ask.reasons.join(" ")).toMatch(/production/);
    expect(ask.reasons.join(" ")).toMatch(/1 open tab/);
    useStore.getState().askConfirm(null);
    ask.onCancel?.();
    expect(await p).toBe(false);
    expect(deleted).toEqual([]);
  });

  it("deletes, clears caches and detaches tabs on confirm", async () => {
    useStore.setState({
      connections: [conn],
      tabs: [{ id: "t", title: "T", sql: "", connection_id: "c1" }],
      schemas: { c1: [], c2: [] },
      objects: { "c1|main": [], "c2|main": [] },
    });
    const p = useStore.getState().deleteConnection("c1");
    useStore.getState().confirm!.onConfirm();
    expect(await p).toBe(true);
    expect(deleted).toEqual(["c1"]);
    const st = useStore.getState();
    expect(st.tabs[0].connection_id).toBeNull();
    expect(Object.keys(st.schemas)).toEqual(["c2"]);
    expect(Object.keys(st.objects)).toEqual(["c2|main"]);
  });
});

describe("dropping outputs", () => {
  it("forgets dropped outputs in runs, output tabs and the list", () => {
    const o1 = out("r1", "t", 0, { state: "live" });
    useStore.setState({
      outputs: [o1, out("r2", "t", 1)],
      tabs: [{ id: "t", title: "T", sql: "" }, { id: "v", title: "r1", sql: "", output_ref: "r1" }],
      activeTabId: "v",
      runs: { t: restoredRun("t", [o1])! },
    });
    useStore.getState().forgetOutputs(["r1"]);
    const st = useStore.getState();
    expect(st.outputs.map((o) => o.handle)).toEqual(["r2"]);
    expect(st.tabs.map((t) => t.id)).toEqual(["t"]);
    expect(st.activeTabId).toBe("t");
    expect(st.runs.t.statements[0].result).toBeUndefined();
    expect(st.runs.t.statements[0].notices.at(-1)).toMatch(/dropped/);
  });
});

describe("sign-in dialog", () => {
  it("closes on its own when the backend signed in by itself, stays for an explicit sign-in", async () => {
    const { nextSignIn } = await import("./store");
    const opened = nextSignIn(null, { type: "browser_opened", url: "https://x" });
    expect(opened?.implicit).toBe(true);
    expect(nextSignIn(opened, { type: "finished" })).toBeNull();
    const explicit = { connectionId: "c", label: "Sign in to dbx", event: null };
    const waiting = nextSignIn(explicit, { type: "browser_opened", url: "https://x" });
    expect(waiting?.label).toBe("Sign in to dbx");
    expect(waiting?.implicit).toBeFalsy();
    expect(nextSignIn(waiting, { type: "finished" })?.event).toEqual({ type: "finished" });
    expect(nextSignIn(null, { type: "finished" })).toBeNull();
  });
});

describe("new notebook", () => {
  it("starts without a connection, in the folder it was created from", async () => {
    useStore.setState({ connections: [{ id: "c1" } as never], tabs: [{ id: "t", title: "q", sql: "", connection_id: "c1" }], activeTabId: "t" });
    await useStore.getState().newNotebook(null, "f1");
    expect(savedNotebooks.at(-1)).toMatchObject({ connection_id: null, folder_id: "f1" });
    await useStore.getState().newNotebook();
    expect(savedNotebooks.at(-1)).toMatchObject({ connection_id: null, folder_id: null });
  });
});

describe("reopen closed tabs", () => {
  it("brings back the last closed tab with its SQL and parameters, in place", () => {
    useStore.setState({
      tabs: [
        { id: "a", title: "A", sql: "select 1", connection_id: "c1" },
        { id: "b", title: "B", sql: "select :d", connection_id: "c1" },
        { id: "c", title: "C", sql: "", connection_id: "c1" },
      ],
      activeTabId: "b",
      closedTabs: [],
      params: { b: { d: { value: "2026-01-01" } } },
    });
    const st = () => useStore.getState();
    st().closeTab("c"); // empty: not remembered
    st().closeTab("b");
    expect(st().closedTabs.map((c) => c.tab.title)).toEqual(["B"]);
    expect(st().reopenClosedTab()).toBe(true);
    const t = st().tabs[1];
    expect([t.title, t.sql, t.id !== "b"]).toEqual(["B", "select :d", true]);
    expect(st().activeTabId).toBe(t.id);
    expect(st().params[t.id]).toEqual({ d: { value: "2026-01-01" } });
    expect(st().reopenClosedTab()).toBe(false);
  });
});
