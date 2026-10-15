import { describe, expect, it, vi } from "vitest";
import type { AgentEvent } from "./lib/types";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

let handler: ((e: AgentEvent) => void) | null = null;
const sent: { session_id?: string | null }[] = [];
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
    aiSend: async (req: { session_id?: string | null }) => {
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
