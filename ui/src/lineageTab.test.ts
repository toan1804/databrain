import { beforeEach, describe, expect, it, vi } from "vitest";

const calls: string[] = [];
vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  onOracleAgent: async () => () => {},
  api: new Proxy({}, { get: (_t, name) => async () => void calls.push(String(name)) }),
}));
vi.mock("@tauri-apps/plugin-clipboard-manager", () => ({ writeText: async () => {} }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ save: async () => null, open: async () => null }));

const { useStore, isQueryTab } = await import("./store");
const { openLineage, openColumnLineage } = await import("./components/LineageTab");

beforeEach(() => {
  calls.length = 0;
  useStore.setState({ tabs: [{ id: "q", title: "Sales", sql: "select a from t", connection_id: "c" }], activeTabId: "q", runs: {} });
});

describe("lineage tab", () => {
  it("opens a view-only tab with the editor's SQL and reuses it", () => {
    openLineage("q");
    const st = useStore.getState();
    const t = st.tabs.find((x) => x.id === st.activeTabId)!;
    expect(t.title).toBe("Lineage · Sales");
    expect(t.lineage).toEqual({ source: "q", base: 0, whole: true });
    expect(t.sql).toBe("select a from t");
    expect(t.connection_id).toBe("c");
    expect(isQueryTab(t)).toBe(false);
    useStore.getState().updateTab("q", { sql: "select b from t" });
    useStore.getState().setActiveTab("q");
    openLineage("q");
    expect(useStore.getState().tabs.filter((x) => x.lineage)).toHaveLength(1);
    expect(useStore.getState().tabs.find((x) => x.lineage)!.sql).toBe("select b from t");
  });

  it("opens one column's flow in its own tab, reused per column", () => {
    openLineage("q");
    const diagram = useStore.getState().tabs.find((x) => x.lineage)!;
    openColumnLineage(diagram, "Result", "total");
    const st = useStore.getState();
    const col = st.tabs.find((x) => x.id === st.activeTabId)!;
    expect(col.title).toBe("Lineage · total");
    expect(col.lineage).toEqual({ source: "q", base: 0, whole: true, column: { node: "Result", column: "total" } });
    expect(isQueryTab(col)).toBe(false);
    openColumnLineage(diagram, "Result", "total");
    openColumnLineage(diagram, "Result", "name");
    expect(useStore.getState().tabs.filter((x) => x.lineage?.column)).toHaveLength(2);
    // "Lineage" from the editor goes back to the diagram tab, not a column tab.
    openLineage("q");
    expect(useStore.getState().activeTabId).toBe(diagram.id);
  });

  it("can't be run; nothing to trace in an empty editor", async () => {
    openLineage("q");
    const id = useStore.getState().activeTabId!;
    await useStore.getState().runTab(id, "all", { doc: "drop table t", selFrom: 0, selTo: 0, cursor: 0 });
    expect(calls).toEqual([]);
    useStore.setState({ tabs: [{ id: "e", title: "Empty", sql: "  ", connection_id: null }], activeTabId: "e" });
    openLineage("e");
    expect(useStore.getState().tabs).toHaveLength(1);
  });
});
