import { describe, expect, it, vi } from "vitest";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  onAiEvent: async () => () => {},
  onKnowledgeEvent: async () => () => {},
  api: {},
}));

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
