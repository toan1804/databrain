import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectionView, CostEstimate } from "./lib/types";

// estimateCost calls wait here until the test answers them.
const pending: { conn: string; sql: string; params: unknown; resolve: (e: CostEstimate) => void; reject: (e: unknown) => void }[] = [];
vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  onOracleAgent: async () => () => {},
  api: {
    statementAtCursor: async (_k: string, doc: string) => ({ sql: doc }),
    estimateCost: (conn: string, _s: string, sql: string, params: unknown) => new Promise<CostEstimate>((resolve, reject) => pending.push({ conn, sql, params, resolve, reject })),
  },
}));
vi.mock("@tauri-apps/plugin-clipboard-manager", () => ({ writeText: async () => {} }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ save: async () => null, open: async () => null }));

const { useStore } = await import("./store");
const { estimateCost, useCost, costKey } = await import("./costPreview");

const conn = (id: string, kind: string): ConnectionView =>
  ({ id, name: id, connected: true, env: "dev", has_secret: false, created_at: 0, updated_at: 0, config: { kind, auth: { kind: "none" } }, ai_policy: {} }) as unknown as ConnectionView;
const gib = (n: number): CostEstimate => ({ method: "dry run", bytes: n * 1024 ** 3, cache_hit: false, notes: [] });
const SQL = "select * from sales.events";
const flush = () => new Promise((r) => setTimeout(r, 0));
const state = () => useCost.getState().byTab.q;
// Starts an estimate and lets it reach the backend (the reply comes when the test says).
const start = async () => {
  void estimateCost("q", true);
  await flush();
};
const switchTo = (id: string) => useStore.getState().updateTab("q", { connection_id: id });

beforeEach(() => {
  pending.length = 0;
  useCost.setState({ byTab: {}, hidden: {} });
  useStore.setState({
    connections: [conn("dev", "bigquery"), conn("prod", "bigquery"), conn("pg", "postgres")],
    tabs: [{ id: "q", title: "Q", sql: SQL, connection_id: "dev" }],
    params: {},
  });
});

describe("cost preview", () => {
  it("estimates again on another connection with the same SQL", async () => {
    await start();
    pending[0].resolve(gib(1.2));
    await flush();
    expect(state()).toMatchObject({ status: "ready", connectionId: "dev" });
    // Same SQL, same connection: skipped.
    await start();
    expect(pending).toHaveLength(1);
    switchTo("prod");
    // The dev number is gone as soon as the tab moves.
    expect(state()).toBeUndefined();
    await start();
    expect(pending).toHaveLength(2);
    expect(pending[1].conn).toBe("prod");
    pending[1].resolve(gib(40));
    await flush();
    expect(state()).toMatchObject({ status: "ready", connectionId: "prod", estimate: { bytes: 40 * 1024 ** 3 } });
  });

  it("drops a reply for the connection the tab has left", async () => {
    await start();
    switchTo("prod");
    await start();
    // prod answers first, then the late dev reply arrives.
    pending[1].resolve(gib(40));
    await flush();
    pending[0].resolve(gib(1.2));
    await flush();
    expect(state()).toMatchObject({ status: "ready", connectionId: "prod", estimate: { bytes: 40 * 1024 ** 3 } });
    // A late failure of the old request is dropped too.
    switchTo("dev");
    await start();
    switchTo("prod");
    pending[2].reject("boom");
    await flush();
    expect(state()).toBeUndefined();
  });

  it("clears the bar on a connection without cost preview", async () => {
    await start();
    pending[0].resolve(gib(1.2));
    await flush();
    useCost.getState().hide("q", SQL);
    switchTo("pg");
    expect(state()).toBeUndefined();
    expect(useCost.getState().hidden.q).toBeUndefined();
    await start();
    expect(pending).toHaveLength(1);
    expect(state()).toBeUndefined();
  });

  it("estimates again when the parameters change", async () => {
    useStore.setState({ params: { q: { day: { value: "2026-01-01" } } } });
    await start();
    pending[0].resolve(gib(1));
    await flush();
    await start();
    expect(pending).toHaveLength(1);
    useStore.setState({ params: { q: { day: { value: "2026-02-01" } } } });
    await start();
    expect(pending).toHaveLength(2);
    expect(pending[1].params).toEqual({ day: { value: "2026-02-01" } });
  });

  it("keys estimates on connection, SQL and parameters", () => {
    expect(costKey("a", SQL)).toBe(costKey("a", SQL, {}));
    expect(costKey("a", SQL)).not.toBe(costKey("b", SQL));
    expect(costKey("a", SQL, { x: { value: "1" }, y: { value: "2" } })).toBe(costKey("a", SQL, { y: { value: "2" }, x: { value: "1" } }));
    expect(costKey("a", SQL, { x: { value: "1" } })).not.toBe(costKey("a", SQL, { x: { value: "2" } }));
  });
});
