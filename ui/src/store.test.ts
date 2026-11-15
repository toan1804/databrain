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
/** While set, run_query waits for it (a slow connect). */
let gate: Promise<void> | null = null;
const cancelled: string[] = [];
const closedTabs: string[] = [];
const reran: string[] = [];

vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  api: {
    runQuery: async (req: RunRequest) => {
      if (gate) await gate;
      const job_id = `job-${++jobSeq}`;
      const plan = { index: 0, sql: req.sql, start: 0, end: req.sql.length, classification: { kind: "read", missing_where: false, keyword: "SELECT" } };
      if (emitBeforeResolve) {
        const t = req.tab_id;
        handler?.({ type: "statement_started", job_id, tab_id: t, index: 0, sql: req.sql });
        handler?.({ type: "statement_finished", job_id, tab_id: t, index: 0, result: null, output: null, rows_affected: 0, duration_ms: 1, notices: [] });
        handler?.({ type: "job_finished", job_id, tab_id: t, status: "success", duration_ms: 1, in_transaction: false });
      }
      return { status: "started", job_id, statements: [plan] };
    },
    cancelQuery: async (id: string) => {
      cancelled.push(id);
      return cancelResult.value;
    },
    closeTab: async (id: string) => {
      closedTabs.push(id);
    },
    // Statement 1 of the tab's run, again: a new output in its place.
    rerunOutput: async (req: RunRequest, handle: string) => {
      reran.push(handle);
      const job_id = `job-${++jobSeq}`;
      const t = req.tab_id;
      const plan = { index: 1, sql: req.sql, start: 0, end: req.sql.length, classification: { kind: "read", missing_where: false, keyword: "SELECT" } };
      const output = { ...out("r9", t, 1), state: "live" };
      handler?.({ type: "statement_started", job_id, tab_id: t, index: 1, sql: req.sql });
      handler?.({ type: "statement_finished", job_id, tab_id: t, index: 1, result: null, output, rows_affected: 0, duration_ms: 3, notices: [] } as never);
      handler?.({ type: "job_finished", job_id, tab_id: t, status: "success", duration_ms: 7, in_transaction: false });
      return { status: "started", job_id, statements: [plan] };
    },
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
    keepOutput: async (handle: string, kept: boolean) => ({ ...outputs.find((o) => o.handle === handle)!, kept }),
  },
}));

const { useStore, _earlyEventCount, restoredRun, shownStatement, RESTORED_NOTICE } = await import("./store");

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

describe("keeping a result across runs", () => {
  it("shows a kept result next to the next run's, until let go", async () => {
    const o1 = out("r1", "t", 0, { state: "live" });
    outputs = [o1];
    useStore.setState({ outputs, runs: { t: restoredRun("t", [o1])! } });
    await useStore.getState().keepResult("t", o1, true);
    expect(useStore.getState().runs.t.statements[0].output?.kept).toBe(true);
    // The next run: r1 moves to the kept results, under a negative index.
    await useStore.getState().runSql("t", "c", "select 2");
    let run = useStore.getState().runs.t;
    expect(run.kept?.map((s) => s.output?.handle)).toEqual(["r1"]);
    const k = run.kept![0].plan.index;
    expect(k).toBeLessThan(0);
    expect(run.statements.map((s) => s.plan.sql)).toEqual(["select 2"]);
    // Shown when picked; still there after another run.
    useStore.getState().setActiveStatement("t", k);
    expect(shownStatement(useStore.getState().runs.t)?.output?.handle).toBe("r1");
    await useStore.getState().runSql("t", "c", "select 3");
    expect(useStore.getState().runs.t.kept?.map((s) => s.plan.index)).toEqual([k]);
    // Let go while shown: it leaves the tab and the latest result is shown.
    useStore.getState().setActiveStatement("t", k);
    await useStore.getState().keepResult("t", o1, false);
    run = useStore.getState().runs.t;
    expect(run.kept).toEqual([]);
    expect(run.activeIndex).toBe(0);
  });

  it("a result that isn't kept is replaced as before", async () => {
    const o1 = out("r1", "t", 0, { state: "live" });
    useStore.setState({ runs: { t: restoredRun("t", [o1])! } });
    await useStore.getState().runSql("t", "c", "select 2");
    expect(useStore.getState().runs.t.kept).toBeUndefined();
  });

  it("restores kept results apart from the tab's last results", () => {
    const run = restoredRun("t", [out("r1", "t", 0, { kept: true, created_at: 1 }), out("r2", "t", 0, { created_at: 2 })])!;
    expect(run.kept?.map((s) => s.output?.handle)).toEqual(["r1"]);
    expect(run.statements.map((s) => s.output?.handle)).toEqual(["r2"]);
    expect(run.activeIndex).toBe(0);
  });

  it("dropping a kept output removes it from the tab", () => {
    const o1 = out("r1", "t", 0, { state: "live", kept: true });
    const run = restoredRun("t", [o1])!;
    useStore.setState({ outputs: [o1], runs: { t: { ...run, activeIndex: run.kept![0].plan.index } } });
    useStore.getState().forgetOutputs(["r1"]);
    expect(useStore.getState().runs.t.kept).toEqual([]);
    expect(useStore.getState().runs.t.activeIndex).toBeNull();
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

describe("closing a tab with unsaved edits", () => {
  it("asks first (close button and Cmd/Ctrl+W), and closes a clean tab at once", () => {
    useStore.setState({
      tabs: [
        { id: "a", title: "A", sql: "select 1", connection_id: "c1", dirty: true, file_path: "/q/a.sql" },
        { id: "b", title: "B", sql: "select 2", connection_id: "c1" },
      ],
      activeTabId: "a",
      closedTabs: [],
      confirm: null,
    });
    const st = () => useStore.getState();
    st().requestCloseTab("a");
    expect(st().tabs.map((t) => t.id)).toEqual(["a", "b"]);
    expect(st().confirm?.reasons[0]).toMatch(/not saved to \/q\/a\.sql/);
    st().confirm!.onConfirm();
    expect(st().tabs.map((t) => t.id)).toEqual(["b"]);
    useStore.setState({ confirm: null });
    st().requestCloseTab("b");
    expect(st().confirm).toBeNull();
    expect(st().tabs.map((t) => t.id)).not.toContain("b");
  });
});

describe("notebook cell runs", () => {
  const run = { jobId: "j", running: false, startedAt: 0, statements: [], activeIndex: null };
  it("are dropped with their cell and with their notebook's tab", () => {
    useStore.setState({
      tabs: [
        { id: "t", title: "Q", sql: "", connection_id: "c1" },
        { id: "nbtab", title: "NB", sql: "", connection_id: "c1", notebook_id: "n1" },
      ],
      activeTabId: "t",
      closedTabs: [],
      runs: { t: run, "nb:n1:c1": run, "nb:n1:c2": run, "nb:n10:c1": run, "nb:n2:c1": run },
    });
    const st = () => useStore.getState();
    st().forgetRuns(["nb:n2:c1"]);
    expect(Object.keys(st().runs).sort()).toEqual(["nb:n10:c1", "nb:n1:c1", "nb:n1:c2", "t"]);
    st().closeTab("nbtab");
    expect(Object.keys(st().runs).sort()).toEqual(["nb:n10:c1", "t"]);
    st().forgetRuns([], "nb:n10:");
    expect(Object.keys(st().runs)).toEqual(["t"]);
  });
});

describe("Run all", () => {
  it("runs the whole script, whatever is selected", async () => {
    const sent: string[] = [];
    const { api } = await import("./lib/api");
    const real = api.runQuery;
    (api as { runQuery: typeof api.runQuery }).runQuery = async (req) => {
      sent.push(req.sql);
      return real(req);
    };
    try {
      const conn = { id: "conn", name: "c", env: "none", config: { kind: "postgres", read_only: false } };
      useStore.setState({ connections: [conn as never], tabs: [{ id: "t1", title: "q", sql: "", connection_id: "conn" } as never], activeTabId: "t1", runs: {} });
      const doc = "delete from t\nwhere id = 1";
      // `delete from t` is selected; Run all must not run it without its WHERE.
      await useStore.getState().runTab("t1", "all", { doc, selFrom: 0, selTo: 13, cursor: 13 });
      expect(sent).toEqual([doc]);
      // Run (the statement or the selection) still runs the selection.
      await useStore.getState().runTab("t1", "statement", { doc, selFrom: 0, selTo: 13, cursor: 13 });
      expect(sent[1]).toBe("delete from t");
    } finally {
      (api as { runQuery: typeof api.runQuery }).runQuery = real;
    }
  });
});

describe("refreshing one result after saving edits", () => {
  it("runs that statement only; the run's other results, status and time stay", async () => {
    const plan = (i: number) => ({ index: i, sql: `select ${i}`, start: i * 10, end: i * 10 + 8, classification: { kind: "read" as const, missing_where: false, keyword: "SELECT" } });
    const done = (i: number) => ({ plan: plan(i), status: "done" as const, notices: [], output: out(`r${i + 1}`, "t", i) });
    useStore.setState({
      tabs: [{ id: "t", title: "q", sql: "", connection_id: "conn" } as never],
      runs: { t: { jobId: "old", running: false, startedAt: 0, finishedStatus: "success", durationMs: 500, statements: [done(0), done(1), done(2)], activeIndex: 2, connectionId: "conn" } },
    });
    expect(await useStore.getState().refreshResult("t", 1)).toBe(true);
    expect(reran).toEqual(["r2"]);
    const run = useStore.getState().runs.t;
    expect(run.statements.map((x) => x.output?.handle)).toEqual(["r1", "r9", "r3"]);
    // Its place in the editor is the statement's own, not offset 0 of the re-run text.
    expect(run.statements[1].plan.start).toBe(10);
    expect(run.activeIndex).toBe(1);
    expect(run.running).toBe(false);
    expect(run.durationMs).toBe(500);
    expect(run.finishedStatus).toBe("success");
    expect(run.refreshOf).toBeUndefined();
  });

  it("does nothing for a statement without a result, or while the tab runs", async () => {
    reran.length = 0;
    useStore.setState({ runs: { t: { jobId: "j", running: true, startedAt: 0, statements: [{ plan: { index: 0 } as never, status: "running", notices: [], output: out("r1", "t", 0) }], activeIndex: 0 } } });
    expect(await useStore.getState().refreshResult("t", 0)).toBe(false);
    expect(await useStore.getState().refreshResult("t", 5)).toBe(false);
    expect(reran).toEqual([]);
  });
});

describe("a tab closed while its run starts", () => {
  it("stops what started and shows nothing for it", async () => {
    emitBeforeResolve = false;
    cancelled.length = 0;
    closedTabs.length = 0;
    let open!: () => void;
    gate = new Promise<void>((r) => (open = r));
    useStore.setState({ tabs: [{ id: "slow", title: "q", sql: "", connection_id: "conn" } as never], runs: {} });
    const p = useStore.getState().runSql("slow", "conn", "update t set a = 1 where id = 1");
    // Cmd+W before run_query answers.
    useStore.setState({ tabs: [] });
    open();
    gate = null;
    expect(await p).toBe(false);
    expect(useStore.getState().runs.slow).toBeUndefined();
    expect(cancelled).toEqual([`job-${jobSeq}`]);
    expect(closedTabs).toContain("slow");
  });

  it("runs as before while the tab stays open", async () => {
    emitBeforeResolve = false;
    cancelled.length = 0;
    useStore.setState({ tabs: [{ id: "open", title: "q", sql: "", connection_id: "conn" } as never], runs: {} });
    expect(await useStore.getState().runSql("open", "conn", "select 1")).toBe(true);
    expect(useStore.getState().runs.open?.running).toBe(true);
    expect(cancelled).toEqual([]);
  });
});

describe("dropping an output", () => {
  it("never leaves no tab open", async () => {
    const o = out("r7", "t", 0, { state: "live" });
    useStore.setState({ outputs: [o], tabs: [{ id: "view", title: "r7", sql: "", connection_id: "conn", output_ref: "r7" } as never], activeTabId: "view", runs: {} });
    useStore.getState().forgetOutputs(["r7"]);
    const { tabs, activeTabId } = useStore.getState();
    expect(tabs).toHaveLength(1);
    expect(tabs[0].output_ref).toBeFalsy();
    expect(activeTabId).toBe(tabs[0].id);
  });
});

describe("refreshing a result of a run restored after a restart", () => {
  it("updates that statement, by its index, not the position", async () => {
    reran.length = 0;
    // Restored: only statements 1 and 3 had results; the refresh is of statement 1 (at position 0).
    const plan = (i: number) => ({ index: i, sql: `select ${i}`, start: i * 10, end: i * 10 + 8, classification: { kind: "read" as const, missing_where: false, keyword: "SELECT" } });
    const done = (i: number) => ({ plan: plan(i), status: "done" as const, notices: [], output: out(`r${i}`, "t", i) });
    useStore.setState({
      tabs: [{ id: "t", title: "q", sql: "", connection_id: "conn" } as never],
      runs: { t: { jobId: null, running: false, startedAt: 0, finishedStatus: "success", statements: [done(1), done(3)], activeIndex: 3, connectionId: "conn" } },
    });
    expect(await useStore.getState().refreshResult("t", 0)).toBe(true);
    const run = useStore.getState().runs.t;
    expect(run.statements.map((x) => x.output?.handle)).toEqual(["r9", "r3"]);
    expect(run.statements.map((x) => x.status)).toEqual(["done", "done"]);
    expect(run.running).toBe(false);
  });
});

describe("a notebook cell deleted while its run starts", () => {
  it("stops what started and shows nothing for it; other cells run as before", async () => {
    emitBeforeResolve = false;
    cancelled.length = 0;
    closedTabs.length = 0;
    useStore.setState({ tabs: [{ id: "nbtab", title: "n", sql: "", connection_id: "conn", notebook_id: "nb1" } as never], runs: {} });
    let open!: () => void;
    gate = new Promise<void>((r) => (open = r));
    const p = useStore.getState().runSql("nb:nb1:cellA", "conn", "update t set a = 1 where id = 1", 0, "nb:nb1");
    // The cell is deleted (its notebook stays open) before run_query answers.
    useStore.getState().retireRunKeys(["nb:nb1:cellA"]);
    open();
    gate = null;
    expect(await p).toBe(false);
    expect(useStore.getState().runs["nb:nb1:cellA"]).toBeUndefined();
    expect(cancelled).toEqual([`job-${jobSeq}`]);
    // The cell's own results; not the notebook's shared session, which the other cells use.
    expect(closedTabs).toEqual(["nb:nb1:cellA"]);
    // Another cell of the same notebook is not affected.
    cancelled.length = 0;
    expect(await useStore.getState().runSql("nb:nb1:cellB", "conn", "select 1", 0, "nb:nb1")).toBe(true);
    expect(useStore.getState().runs["nb:nb1:cellB"]?.running).toBe(true);
    expect(cancelled).toEqual([]);
  });
});
