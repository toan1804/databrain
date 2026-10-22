import { describe, expect, it, vi } from "vitest";
import type { AgentEvent } from "./lib/types";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

let handler: ((e: AgentEvent) => void) | null = null;
const sent: { session_id?: string | null; context?: { connections?: string[] } }[] = [];
let n = 0;
const responses: { request_id: string; body: unknown }[] = [];
const cancelled: string[] = [];
/** When set, ai_send only returns the run id; the test sends the events. */
let manual = false;

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
    aiCancel: async (run_id: string) => {
      cancelled.push(run_id);
      return true;
    },
    aiSessions: async () => [],
    aiSessionMessages: async () => [],
    aiRespond: async (request_id: string, body: unknown) => {
      responses.push({ request_id, body });
    },
    setSetting: async () => {},
    aiListProviders: async () => [{ id: "p", name: "P", kind: "openai", config: {}, has_key: true }],
    aiSend: async (req: (typeof sent)[number]) => {
      sent.push(req);
      const run_id = `run${++n}`;
      if (manual) return run_id;
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
    const shown = (patch: Record<string, unknown>) => {
      const s = useAi.getState();
      useAi.setState({ chats: s.chats.map((c) => (c.id === s.activeChatId ? { ...c, ...patch } : c)), ...patch });
    };
    shown({ runId: "rx", running: true, items: [], autoApprove: false });
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
    // (The first conversation keeps its run in its own tab; this one has another.)
    shown({ runId: "ry", running: true });
    handler?.({ type: "approval_request", run_id: "ry", request_id: "q3", tool: "run_query", summary: "write", detail: { sql: "delete from t" } } as unknown as AgentEvent);
    expect(responses.find((r) => r.request_id === "q3")).toBeUndefined();
  });
});

describe("several conversations", () => {
  it("work at the same time, each with its own run, answer and approvals", async () => {
    const { useAi } = await import("./aiStore");
    manual = true;
    const ai = () => useAi.getState();
    const a = ai().newChat();
    await ai().send({ chatId: a, message: "question A", connectionId: "c1", targetKey: null });
    // B starts while A is still working: it is not queued behind A.
    const b = ai().newChat();
    expect(b).not.toBe(a);
    await ai().send({ chatId: b, message: "question B", connectionId: "c1", targetKey: null });
    const chatA = () => ai().chats.find((c) => c.id === a)!;
    const chatB = () => ai().chats.find((c) => c.id === b)!;
    expect(chatA().running && chatB().running).toBe(true);
    expect(chatB().queued).toEqual([]);
    const [runA, runB] = [chatA().runId!, chatB().runId!];
    expect(runA).not.toBe(runB);

    // Events go to the conversation of their run.
    handler?.({ type: "started", session_id: "sA", run_id: runA } as AgentEvent);
    handler?.({ type: "started", session_id: "sB", run_id: runB } as AgentEvent);
    handler?.({ type: "text_delta", session_id: "sA", run_id: runA, text: "answer A" } as AgentEvent);
    handler?.({ type: "text_delta", session_id: "sB", run_id: runB, text: "answer B" } as AgentEvent);
    handler?.({ type: "approval_request", run_id: runA, request_id: "ra", tool: "run_query", summary: "", detail: {} } as unknown as AgentEvent);
    const texts = (id: string) => ai().chats.find((c) => c.id === id)!.items.filter((i) => i.kind === "assistant").map((i) => (i as { text: string }).text);
    expect(texts(a)).toEqual(["answer A"]);
    expect(texts(b)).toEqual(["answer B"]);
    expect(chatA().items.some((i) => i.kind === "approval")).toBe(true);
    expect(chatB().items.some((i) => i.kind === "approval")).toBe(false);
    expect([chatA().sessionId, chatB().sessionId]).toEqual(["sA", "sB"]);

    // B is shown; A finishing in the background is marked unread.
    expect(ai().activeChatId).toBe(b);
    handler?.({ type: "finished", session_id: "sA", run_id: runA, text: "answer A" } as AgentEvent);
    expect(chatA().running).toBe(false);
    expect(chatA().unread).toBe(true);
    expect(chatB().running).toBe(true);
    ai().switchChat(a);
    expect(ai().activeChatId).toBe(a);
    expect(chatA().unread).toBe(false);
    expect(ai().items).toBe(chatA().items);

    // Closing B stops its run; A stays.
    ai().closeChat(b);
    expect(cancelled).toContain(runB);
    expect(ai().chats.some((c) => c.id === b)).toBe(false);
    expect(ai().activeChatId).toBe(a);
    manual = false;
  });

  it("a new chat in the background leaves the shown one shown", async () => {
    const { useAi } = await import("./aiStore");
    const shown = useAi.getState().activeChatId;
    const id = useAi.getState().newChat({ activate: false });
    expect(useAi.getState().activeChatId).toBe(shown);
    expect(useAi.getState().chats.some((c) => c.id === id)).toBe(true);
  });

  it("opening a history entry that is open shows its tab", async () => {
    const { useAi } = await import("./aiStore");
    const open = useAi.getState().chats.find((c) => c.sessionId === "sA")!;
    const n = useAi.getState().chats.length;
    useAi.getState().newChat();
    await useAi.getState().loadSession("sA");
    expect(useAi.getState().activeChatId).toBe(open.id);
    expect(useAi.getState().chats.length).toBeLessThanOrEqual(n + 1);
  });
});
