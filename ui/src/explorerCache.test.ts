import { beforeEach, describe, expect, it, vi } from "vitest";
import type { CachedExplorer, DbObject, SchemaInfo } from "./lib/types";

// Fake backend: a local explorer cache plus a server that can be down or slow.
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const calls: string[] = [];
let online = true;
let cache: CachedExplorer | null = null;
const cachedListings: Record<string, DbObject[]> = {};
const cachedCols: Record<string, { columns: { name: string }[]; stale: boolean }> = {};
const t = (schema: string, name: string): DbObject => ({ schema, name, kind: "table" });
const sch = (catalog: string, s: string, is_default = false): SchemaInfo => ({ name: `${catalog}.${s}`, catalog, is_default });
const live = <T,>(name: string, v: T) => {
  calls.push(name);
  return online ? sleep(5).then(() => v) : Promise.reject(new Error("Connection refused by db:5432"));
};

vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "connection", message: e instanceof Error ? e.message : String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  onOracleAgent: async () => () => {},
  api: {
    listConnections: async () => [],
    cachedExplorer: async () => cache ?? { catalogs: null, schemas: null, catalog_schemas: {}, state: { checked_at: null, catalogs_at: null, schemas_at: null, listed_schemas: 0 } },
    cachedObjects: async (_id: string, s: string) => (cachedListings[s] ? { objects: cachedListings[s], listed_at: 1 } : null),
    cachedColumns: async (_id: string, s: string, n: string) => cachedCols[`${s}.${n}`] ?? null,
    listCatalogs: () => live("listCatalogs", [{ name: "main", is_default: true }, { name: "dev", is_default: false }]),
    listCatalogSchemas: (_id: string, c: string) => live(`listCatalogSchemas:${c}`, c === "main" ? [sch("main", "sales", true), sch("main", "hr")] : [sch("dev", "x")]),
    listSchemas: () => live("listSchemas", [{ name: "public", is_default: true }]),
    listObjects: (_id: string, s: string) => live(`listObjects:${s}`, [t(s, "fresh")]),
    refreshSchema: (_id: string, s: string) => live(`refreshSchema:${s}`, { schema: s, mode: "incremental", added: 1, changed: 0, removed: 0, objects: [t(s, "orders"), t(s, "new_one")] }),
    revalidateExplorer: (_id: string, open: string[]) =>
      live(`revalidate:${open.join(",")}`, { refreshed: !cache ? [] : [{ schema: "main.sales", mode: "incremental", added: 1, changed: 0, removed: 0, objects: [t("main.sales", "orders"), t("main.sales", "new_one")] }], unchanged: 3, deferred: [], fingerprints: true, skipped: false }),
    describeObject: (_id: string, s: string, n: string) => live(`describe:${s}.${n}`, { columns: [{ name: "id" }, { name: "added_col" }] }),
  },
}));

const { useStore } = await import("./store");
const conn = { id: "c", name: "dbx", connected: false, config: { kind: "databricks", auth: { method: "token" }, options: {} } };

beforeEach(() => {
  calls.length = 0;
  online = true;
  cache = null;
  for (const k of Object.keys(cachedListings)) delete cachedListings[k];
  for (const k of Object.keys(cachedCols)) delete cachedCols[k];
  useStore.setState({ connections: [conn as never], schemas: {}, objects: {}, columns: {}, catalogs: {}, catalogSchemas: {}, explorer: {}, treeOpen: {} });
});

const settle = () => sleep(60);

describe("explorer cache", () => {
  it("lists catalogs first, schemas of a catalog when opened, and prefetches the default catalog in the background", async () => {
    await useStore.getState().openExplorer("c");
    expect(useStore.getState().catalogs.c.map((c) => c.name)).toEqual(["main", "dev"]);
    expect(calls).toContain("listCatalogs");
    expect(calls).not.toContain("listSchemas");
    await settle();
    // Default catalog's schemas, then its default schema's tables, without a click.
    expect(calls).toContain("listCatalogSchemas:main");
    expect(calls).not.toContain("listCatalogSchemas:dev");
    await settle();
    expect(calls, JSON.stringify(calls)).toContain("listObjects:main.sales");
    expect(calls).toContain("listObjects:main.hr");
    expect(useStore.getState().objects["c|main.sales"]).toBeDefined();
    expect(useStore.getState().schemas.c.map((s) => s.name)).toEqual(["main.sales", "main.hr"]);
  });

  it("shows the cache at once, then applies only the changes", async () => {
    cache = {
      catalogs: [{ name: "main", is_default: true }],
      schemas: null,
      catalog_schemas: { main: [sch("main", "sales", true)] },
      state: { checked_at: 1, catalogs_at: 1, schemas_at: null, listed_schemas: 1 },
    };
    cachedListings["main.sales"] = [t("main.sales", "orders")];
    useStore.setState({ treeOpen: { "s|c|main.sales": true } });
    online = false;
    const p = useStore.getState().openExplorer("c");
    await p;
    // Offline: the cached tree, labelled, no error.
    expect(useStore.getState().catalogs.c.map((c) => c.name)).toEqual(["main"]);
    expect(useStore.getState().catalogSchemas["c|main"].length).toBe(1);
    expect(useStore.getState().explorer.c.offline).toMatch(/refused/);
    expect(await useStore.getState().loadObjects("c", "main.sales")).toEqual([t("main.sales", "orders")]);
    expect(calls).not.toContain("listObjects:main.sales");

    // Back online: one check, the changed schema is updated from its diff.
    online = true;
    await useStore.getState().openExplorer("c");
    await settle();
    expect(useStore.getState().explorer.c.offline).toBeNull();
    expect(calls).toContain("revalidate:main.sales");
    expect(useStore.getState().objects["c|main.sales"].map((o) => o.name)).toEqual(["orders", "new_one"]);
  });

  it("throws when there is neither a cache nor a server", async () => {
    online = false;
    await expect(useStore.getState().openExplorer("c")).rejects.toThrow(/refused/);
  });

  it("refresh fetches only changes; stale cached columns show then update", async () => {
    useStore.setState({ explorer: { c: { fingerprints: true } } });
    const objs = await useStore.getState().loadObjects("c", "main.sales", true);
    expect(calls).toEqual(["refreshSchema:main.sales"]);
    expect(objs.map((o) => o.name)).toEqual(["orders", "new_one"]);

    cachedCols["main.sales.orders"] = { columns: [{ name: "id" }], stale: true };
    const cols = await useStore.getState().loadColumns("c", "main.sales", "orders");
    expect(cols.map((c) => c.name)).toEqual(["id"]);
    await settle();
    expect(useStore.getState().columns["c|main.sales|orders"].map((c) => c.name)).toEqual(["id", "added_col"]);

    // Fresh cached columns: no server call.
    calls.length = 0;
    cachedCols["main.sales.x"] = { columns: [{ name: "a" }], stale: false };
    await useStore.getState().loadColumns("c", "main.sales", "x");
    await settle();
    expect(calls).toEqual([]);
  });
});
