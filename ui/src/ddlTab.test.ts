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
// Clipboard plugin is not needed (Copy DDL is gone from the menu).
vi.mock("@tauri-apps/plugin-clipboard-manager", () => ({ writeText: async () => {} }));

const { useStore, isQueryTab } = await import("./store");
const { showDdl } = await import("./components/CatalogMenus");

const conn = { id: "c", name: "pg", connected: true, config: { kind: "postgres", auth: { method: "password", user: "u" }, options: {} } } as never;
const obj = { schema: "public", name: "orders", kind: "table" } as const;

beforeEach(() => {
  calls.length = 0;
  useStore.setState({ connections: [conn], tabs: [{ id: "q", title: "Query 1", sql: "select 1", connection_id: "c" }], activeTabId: "q", runs: {} });
});

describe("Show DDL", () => {
  it("opens a read-only tab at once, before any server call, and reuses it", () => {
    showDdl(conn, obj);
    const st = useStore.getState();
    const tab = st.tabs.find((t) => t.id === st.activeTabId)!;
    expect(tab.ddl).toEqual({ schema: "public", name: "orders", kind: "table" });
    expect(tab.title).toBe("orders DDL");
    expect(isQueryTab(tab)).toBe(false);
    expect(calls).toEqual([]); // the tab itself loads the DDL
    useStore.getState().setActiveTab("q");
    showDdl(conn, obj);
    expect(useStore.getState().tabs.filter((t) => t.ddl).length).toBe(1);
    expect(useStore.getState().activeTabId).toBe(tab.id);
    showDdl(conn, { ...obj, name: "customers" });
    expect(useStore.getState().tabs.filter((t) => t.ddl).length).toBe(2);
  });

  it("can't be run or saved as a query", async () => {
    showDdl(conn, obj);
    const id = useStore.getState().activeTabId!;
    await useStore.getState().runTab(id, "all", { doc: "drop table orders", selFrom: 0, selTo: 0, cursor: 0 });
    await useStore.getState().saveTabQuery(id);
    expect(calls).toEqual([]);
    expect(useStore.getState().runs[id]).toBeUndefined();
  });
});
