import { beforeEach, describe, expect, it, vi } from "vitest";

const calls: string[] = [];
let tabs: { id: string; kind?: string; sql: string; connection_id: string | null }[] = [];
const fake = {
  connections: [{ id: "pg", config: { kind: "postgres" } }],
  get tabs() {
    return tabs;
  },
  refreshConnections: async () => void calls.push("refresh"),
  toast: (m: string) => void calls.push(`toast ${m}`),
  newTab: (init: { sql: string; connection_id: string }) => {
    const id = `t${tabs.length + 1}`;
    tabs.push({ id, sql: init.sql, connection_id: init.connection_id });
    calls.push(`new ${init.sql}`);
    return id;
  },
  openSqlFile: async (path: string) => {
    if (path.endsWith("bad.sql")) return undefined;
    const id = `t${tabs.length + 1}`;
    tabs.push({ id, sql: `-- ${path}`, connection_id: null });
    calls.push(`file ${path}`);
    return id;
  },
  updateTab: (id: string, patch: { connection_id: string }) => {
    tabs = tabs.map((t) => (t.id === id ? { ...t, ...patch } : t));
  },
  setParam: (id: string, name: string, v: { value: string }) => void calls.push(`param ${id} ${name}=${v.value}`),
  runTab: async (id: string, mode: string, input: { doc: string }) => {
    calls.push(`run ${id} ${mode} on ${tabs.find((t) => t.id === id)?.connection_id}: ${input.doc}`);
  },
};

vi.mock("./store", () => ({ useStore: { getState: () => fake } }));
vi.mock("./store/types", () => ({ isQueryTab: (t: unknown) => !!t }));
vi.mock("./editorBridge", () => ({ editorBridge: { get: () => undefined } }));
vi.mock("./components/sidebar/queryLocalFile", () => ({
  openLocalFile: async (p: string) => void calls.push(`data ${p}`),
  localFilesConnection: async () => ({ id: "local" }),
}));
vi.mock("./lib/api", () => ({ api: {}, isTauri: () => false, onDbrOpen: async () => () => {}, toError: (e: unknown) => ({ message: String(e) }) }));

describe("dbr opens", () => {
  beforeEach(() => {
    calls.length = 0;
    tabs = [];
  });

  it("opens data as results and runs SQL and .sql files on the connection, in order", async () => {
    const { applyDbrOpen } = await import("./dbrOpen");
    await applyDbrOpen({
      connection_id: "pg",
      items: [
        { kind: "data", path: "/d/orders.csv" },
        { kind: "sql_file", path: "/d/report.sql" },
        { kind: "sql", text: "select :day" },
      ],
      params: { day: { value: "1", raw: false } },
    });
    expect(calls).toEqual([
      "data /d/orders.csv",
      "file /d/report.sql",
      "param t1 day=1",
      "run t1 all on pg: -- /d/report.sql",
      "new select :day",
      "param t2 day=1",
      "run t2 all on pg: select :day",
    ]);
  });

  it("runs on Local files without a connection, and skips a file that didn't open", async () => {
    const { applyDbrOpen } = await import("./dbrOpen");
    await applyDbrOpen({ connection_id: null, items: [{ kind: "sql_file", path: "/d/bad.sql" }, { kind: "sql", text: "select 1" }], params: {} });
    expect(calls).toEqual(["new select 1", "run t1 all on local: select 1"]);
  });

  it("opens nothing when its connection is gone", async () => {
    const { applyDbrOpen } = await import("./dbrOpen");
    await applyDbrOpen({ connection_id: "gone", items: [{ kind: "sql", text: "select 1" }], params: {} });
    expect(calls).toEqual(["refresh", "toast dbr: its connection is no longer saved"]);
  });
});
