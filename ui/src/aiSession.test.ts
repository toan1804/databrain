import { describe, expect, it, vi } from "vitest";
import type { AgentEvent } from "./lib/types";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

let handler: ((e: AgentEvent) => void) | null = null;
const sent: { session_id?: string | null; context?: { connections?: string[] } }[] = [];
let n = 0;

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
