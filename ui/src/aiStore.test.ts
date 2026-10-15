import { describe, expect, it, vi } from "vitest";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  onAiEvent: async () => () => {},
  onKnowledgeEvent: async () => () => {},
  api: {
    setSetting: async () => {},
    aiListModels: async (id: string) => {
      modelCalls.push(id);
      return [{ id: `${id}-a` }, { id: `${id}-b` }];
    },
  },
}));
const modelCalls: string[] = [];

describe("aiStore: provider settings and models", () => {
  it("offers provider settings for sign-in and key errors only", async () => {
    const { needsProviderSettings } = await import("./aiStore");
    expect(needsProviderSettings({ kind: "auth", message: "Kiro: Not signed in. Use “Sign in with browser”." })).toBe(true);
    expect(needsProviderSettings({ kind: "provider", message: "HTTP 401: invalid api key" })).toBe(true);
    expect(needsProviderSettings({ kind: "config", message: "choose a model for \"OpenAI\"" })).toBe(true);
    expect(needsProviderSettings({ kind: "provider", message: "rate limited, retry later" })).toBe(false);
    expect(needsProviderSettings({ kind: "policy", message: "provider \"x\" is not allowed for this connection" })).toBe(false);
  });

  it("loads a provider's models once and shares them", async () => {
    const { useAi } = await import("./aiStore");
    const [a, b] = await Promise.all([useAi.getState().loadModels("p1"), useAi.getState().loadModels("p1")]);
    expect(a).toEqual(["p1-a", "p1-b"]);
    expect(b).toBe(a);
    // Picking a model does not drop the list.
    useAi.getState().setProvider("p1", "p1-b");
    expect(useAi.getState().models.p1).toEqual(["p1-a", "p1-b"]);
    await useAi.getState().loadModels("p1");
    expect(modelCalls).toEqual(["p1"]);
    await useAi.getState().loadModels("p1", true);
    expect(modelCalls).toEqual(["p1", "p1"]);
  });
});

describe("aiStore: chat items", () => {
  it("shows a tool waiting for approval once, as the approval card", async () => {
    const { visibleItems } = await import("./aiStore");
    const items = [
      { id: "1", kind: "tool" as const, callId: "c1", tool: "run_query", args: { sql: "select 1" }, done: false },
      { id: "2", kind: "approval" as const, requestId: "r1", tool: "run_query", summary: "", detail: { sql: "select 1" }, state: "pending" as const },
    ];
    expect(visibleItems(items).map((i) => i.id)).toEqual(["2"]);
    // After approving, the running card shows again (spinner, then the result).
    const approved = [items[0], { ...items[1], state: "approved" as const }];
    expect(visibleItems(approved).map((i) => i.id)).toEqual(["1", "2"]);
  });
});

describe("aiStore: failed statement", () => {
  const doc = "select 1;\n\nselec 2 from t;\n";
  it("finds the failed statement at its run offsets, then by text when the doc moved", async () => {
    const { locateStatement, lineOf } = await import("./aiStore");
    const at = locateStatement(doc, { sql: "selec 2 from t", start: 11, end: 25 })!;
    expect(doc.slice(at.from, at.to)).toBe("selec 2 from t");
    expect(lineOf(doc, at.from)).toBe(3);
    // Text inserted above since the run: still found, nearest to the old offset.
    const moved = "-- note\n" + doc;
    const m = locateStatement(moved, { sql: "selec 2 from t", start: 11, end: 25 })!;
    expect(moved.slice(m.from, m.to)).toBe("selec 2 from t");
    expect(lineOf(moved, m.from)).toBe(4);
    // Edited away: not found.
    expect(locateStatement("select 1;", { sql: "selec 2 from t", start: 11, end: 25 })).toBeNull();
  });
});

describe("aiStore: while the assistant works", () => {
  it("queues a new question instead of dropping it", async () => {
    const { useAi } = await import("./aiStore");
    const { useStore } = await import("./store");
    useStore.setState({ backendAvailable: true });
    useAi.setState({ running: true, queued: [] });
    await useAi.getState().send({ message: "and by month?" });
    expect(useAi.getState().queued.map((q) => q.message)).toEqual(["and by month?"]);
    useAi.getState().removeQueued(useAi.getState().queued[0].id);
    expect(useAi.getState().queued).toEqual([]);
    useAi.setState({ running: false });
  });

  it("says what it is doing", async () => {
    const { workingStatus } = await import("./aiStore");
    expect(workingStatus([]).text).toBe("Thinking…");
    expect(workingStatus([{ id: "1", kind: "tool", callId: "c", tool: "run_query", args: {}, done: false }]).text).toBe("Working: run query…");
    expect(workingStatus([{ id: "1", kind: "assistant", text: "x", streaming: true }]).text).toBe("Writing the answer…");
    const wait = workingStatus([{ id: "1", kind: "approval", requestId: "r", tool: "run_query", summary: "", detail: {}, state: "pending" }]);
    expect(wait.waitingForYou).toBe(true);
  });
});

describe("aiStore: resend a failed question", () => {
  it("marks errors after signing in again and sends the question again", async () => {
    const { useAi } = await import("./aiStore");
    const retry = { message: "top customers?" };
    useAi.setState({
      running: true,
      queued: [],
      items: [{ id: "e1", kind: "error", text: "Kiro: Not signed in", providerId: "p1", retry }],
    });
    useAi.getState().markReauthed("p1");
    const e = useAi.getState().items[0];
    expect(e.kind === "error" && e.reauthed).toBe(true);
    // Still busy: the question waits instead of being dropped.
    useAi.getState().resend("e1");
    expect(useAi.getState().items).toEqual([]);
    expect(useAi.getState().queued.map((q) => q.message)).toEqual(["top customers?"]);
    useAi.setState({ running: false, queued: [] });
  });
});
