import { describe, expect, it, vi } from "vitest";
import type { AgentEvent } from "./lib/types";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

let handler: ((e: AgentEvent) => void) | null = null;
const sent: { session_id?: string | null; context?: { connections?: string[] } }[] = [];
let n = 0;
const responses: { request_id: string; body: unknown }[] = [];

vi.mock("./lib/api", () => ({
  isTauri: () => true,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  onAiEvent: async (h: (e: AgentEvent) => void) => {
    handler = h;
    return () => {};
  },
  onKnowledgeEvent: async () => () => {},
  api: {
    getSettings: async () => ({}),
    aiRespond: async (request_id: string, body: unknown) => {
      responses.push({ request_id, body });
    },
    setSetting: async () => {},
    aiListProviders: async () => [{ id: "p", name: "P", kind: "openai", config: {}, has_key: true }],
    aiSend: async (req: (typeof sent)[number]) => {
      sent.push(req);
      const run_id = `run${++n}`;
      // A fast backend: the run starts (and finishes) before ai_send returns.
      handler?.({ type: "started", session_id: "sess-1", run_id } as AgentEvent);
      handler?.({ type: "finished", session_id: "sess-1", run_id, text: "ok" } as AgentEvent);
      return run_id;
    },
  },
}));

describe("AI conversation memory", () => {
  it("keeps the session when the run's first events beat ai_send's reply", async () => {
    const { useStore } = await import("./store");
    const { useAi } = await import("./aiStore");
    useStore.setState({ backendAvailable: true });
    await useAi.getState().init();
    await useAi.getState().send({ message: "top customers?", connectionId: "c1", targetKey: null });
    expect(useAi.getState().sessionId).toBe("sess-1");
    expect(useAi.getState().running).toBe(false);
    // The next question continues it, also on another connection.
    await useAi.getState().send({ message: "and by month?", connectionId: "c2", targetKey: null });
    expect(sent.map((r) => r.session_id)).toEqual([null, "sess-1"]);
  });
});

describe("AI on several connections", () => {
  it("sends the other chosen connections that still have AI on", async () => {
    const { useStore } = await import("./store");
    const { useAi } = await import("./aiStore");
    const conn = (id: string, ai: boolean) => ({ id, name: id, ai_policy: { ai_enabled: ai } });
    useStore.setState({ connections: [conn("c1", true), conn("c2", true), conn("off", false)] as never });
    useAi.getState().setAlsoConnections(["c2", "off", "gone", "c1"]);
    await useAi.getState().send({ message: "compare both", connectionId: "c1", targetKey: null });
    expect(sent.at(-1)?.context?.connections).toEqual(["c2"]);
  });
});

describe("auto-approve", () => {
  it("approves this conversation's requests, and asks again in a new chat", async () => {
    const { useAi } = await import("./aiStore");
    const ask = (id: string) => handler?.({ type: "approval_request", run_id: "rx", request_id: id, tool: "run_query", summary: "write", detail: { sql: "delete from t" } } as unknown as AgentEvent);
    useAi.setState({ runId: "rx", running: true, items: [], autoApprove: false });
    ask("q1");
    expect(responses.find((r) => r.request_id === "q1")).toBeUndefined();
    // Turning it on approves what is waiting, and what comes next.
    useAi.getState().setAutoApprove(true);
    ask("q2");
    await Promise.resolve();
    expect(responses.filter((r) => ["q1", "q2"].includes(r.request_id)).map((r) => (r.body as { approved: boolean }).approved)).toEqual([true, true]);
    expect(useAi.getState().items.filter((i) => i.kind === "approval").every((i) => i.kind === "approval" && i.state === "approved" && i.auto)).toBe(true);
    // A new conversation asks again.
    useAi.getState().newChat();
    expect(useAi.getState().autoApprove).toBe(false);
    useAi.setState({ runId: "rx", running: true });
    ask("q3");
    expect(responses.find((r) => r.request_id === "q3")).toBeUndefined();
  });
});
