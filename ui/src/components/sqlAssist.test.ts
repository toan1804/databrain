import { beforeEach, describe, expect, it, vi } from "vitest";
import type { DbObject } from "../lib/types";

const t = (schema: string, name: string): DbObject => ({ schema, name, kind: "table" });
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

const calls = { remote: [] as (string | null)[][], local: [] as (string | null)[][], describe: 0, loadObjects: 0 };
let remoteDelay = 0;
let remoteRows: DbObject[] = [];
let localRows: DbObject[] = [];

vi.mock("../lib/api", () => ({
  api: {
    completeTablesLocal: async (_id: string, schema: string | null, q: string) => {
      calls.local.push([schema, q]);
      return localRows.filter((o) => o.name.includes(q.toLowerCase()));
    },
    completeTables: async (_id: string, schema: string | null, q: string) => {
      calls.remote.push([schema, q]);
      await sleep(remoteDelay);
      return remoteRows.filter((o) => o.name.includes(q.toLowerCase()));
    },
    completeColumnsLocal: async () => null,
  },
}));

const state = {
  connections: [{ id: "c", connected: true, config: { kind: "databricks", auth: { method: "token" } } }],
  schemas: { c: [{ name: "main.sales", catalog: "main", is_default: true }] } as Record<string, unknown>,
  objects: {} as Record<string, DbObject[]>,
  columns: {} as Record<string, { name: string }[]>,
  outputs: [],
  loadSchemas: async () => [],
  loadObjects: async () => {
    calls.loadObjects++;
    return [];
  },
  loadColumns: async () => {
    calls.describe++;
    await sleep(remoteDelay);
    return [{ name: "id" }, { name: "amount" }];
  },
};
vi.mock("../store", () => ({ useStore: { getState: () => state, subscribe: () => () => {} } }));

describe("completion metadata", () => {
  beforeEach(() => {
    calls.remote = [];
    calls.local = [];
    calls.describe = 0;
    calls.loadObjects = 0;
    remoteDelay = 0;
  });

  it("searches one schema on the server instead of loading it whole", async () => {
    const { storeProvider } = await import("./sqlAssist");
    remoteRows = [t("main.sales", "orders"), t("main.sales", "order_items"), t("main.sales", "refunds")];
    localRows = [];
    const p = storeProvider("c")!;
    const got = await p.objects("main.sales", "ord");
    expect(got?.map((o) => o.name)).toEqual(["orders", "order_items"]);
    expect(calls.remote).toEqual([["main.sales", "ord"]]);
    expect(calls.loadObjects).toBe(0);
    // A longer prefix is answered from the finished shorter search (it was complete).
    const more = await p.objects("main.sales", "order_");
    expect(more?.map((o) => o.name)).toEqual(["order_items"]);
    expect(calls.remote).toHaveLength(1);
  });

  it("shows the local index at once when the server is slow, then refreshes", async () => {
    const { storeProvider } = await import("./sqlAssist");
    remoteRows = [t("main.sales", "customers"), t("main.crm", "customer_notes")];
    localRows = [t("main.sales", "customers")];
    remoteDelay = 400;
    const late = vi.fn();
    const p = storeProvider("c", late)!;
    const start = Date.now();
    const first = await p.searchTables("cust");
    expect(Date.now() - start).toBeLessThan(350);
    expect(first.map((o) => o.name)).toEqual(["customers"]);
    await sleep(450);
    expect(late).toHaveBeenCalledTimes(1);
    // The next request (re-opened list) has the server's answer.
    const second = await p.searchTables("cust");
    expect(second.map((o) => o.name)).toEqual(["customers", "customer_notes"]);
    expect(calls.remote).toHaveLength(1);
  });

  it("loads a table's columns once, however fast the typing", async () => {
    const { storeProvider } = await import("./sqlAssist");
    remoteDelay = 300;
    const late = vi.fn();
    const p = storeProvider("c", late)!;
    const r = await Promise.all([p.columns("main.sales", "orders"), p.columns("main.sales", "orders"), p.columns("main.sales", "orders")]);
    expect(r).toEqual([undefined, undefined, undefined]);
    expect(calls.describe).toBe(1);
    await sleep(350);
    expect(late).toHaveBeenCalled();
  });
});
