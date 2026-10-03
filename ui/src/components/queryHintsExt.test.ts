import { describe, expect, it, vi } from "vitest";
import type { TableLayout } from "../lib/types";

const calls: string[] = [];
const layouts: Record<string, TableLayout> = {
  "public.events": { indexes: [], partition_by: ["event_date"], partition_kind: "range", cluster_by: [], requires_partition_filter: false, notes: [] },
};

vi.mock("../lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  api: {
    tableLayout: async (_id: string, schema: string, name: string) => {
      calls.push(`${schema}.${name}`);
      const l = layouts[`${schema}.${name}`];
      if (!l) throw new Error("not found");
      return l;
    },
    listObjects: async () => [],
    listSchemas: async () => [{ name: "public", is_default: true }],
    searchObjects: async () => [],
  },
}));

describe("queryHintsExt: analyze", () => {
  it("resolves tables through the store, loads layouts once and offsets hints per statement", async () => {
    const { useStore } = await import("../store");
    const { analyze } = await import("./queryHintsExt");
    useStore.setState({
      connections: [{ id: "c1", connected: true, config: { kind: "postgres", auth: { method: "password" } } } as never],
      schemas: { c1: [{ name: "public", is_default: true }] },
      objects: { "c1|public": [{ schema: "public", name: "events", kind: "table" }] },
    });
    const doc = "select 1;\nselect * from events where user_id = 1;\nselect * from events where event_date > '2024-01-01'";
    const a = await analyze(doc, "c1");
    expect(a.hints).toHaveLength(1);
    expect(doc.slice(a.hints[0].from, a.hints[0].to)).toBe("events");
    expect(a.hints[0].from).toBe(doc.indexOf("events"));
    expect(a.tables).toHaveLength(2);
    expect(calls).toEqual(["public.events"]);
  });

  it("does nothing for connections that would need an interactive sign-in", async () => {
    const { useStore } = await import("../store");
    const { analyze } = await import("./queryHintsExt");
    useStore.setState({ connections: [{ id: "c2", connected: false, config: { kind: "postgres", auth: { method: "oauth_browser" } } } as never] });
    expect((await analyze("select * from events", "c2")).hints).toEqual([]);
  });
});
