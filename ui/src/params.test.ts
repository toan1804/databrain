import { beforeEach, describe, expect, it, vi } from "vitest";
import type { RunRequest } from "./lib/types";

const sent: RunRequest[] = [];
const settings: Record<string, unknown> = {};
vi.mock("./lib/api", () => ({
  isTauri: () => true,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  onOracleAgent: async () => () => {},
  api: {
    // Same rule as the backend for these cases.
    sqlParameters: async (_k: string, text: string) =>
      [...text.matchAll(/(?<![:\w]):([A-Za-z_]\w*)/g)].map((m) => ({ name: m[1], from: m.index!, to: m.index! + m[0].length })),
    runQuery: async (req: RunRequest) => {
      sent.push(req);
      return { status: "started", job_id: "j", statements: [] };
    },
    setSetting: async (k: string, v: unknown) => void (settings[k] = v),
    closeTab: async () => {},
  },
}));

const { useStore } = await import("./store");
const conn = { id: "c", name: "pg", connected: true, config: { kind: "postgres", auth: { method: "password", user: "u" }, options: {} } } as never;
const input = (doc: string) => ({ doc, selFrom: 0, selTo: 0, cursor: 0 });

beforeEach(() => {
  sent.length = 0;
  useStore.setState({ connections: [conn], tabs: [{ id: "t", title: "Q", sql: "", connection_id: "c" }], activeTabId: "t", runs: {}, params: {}, paramFocus: null, backendAvailable: true });
});

describe("query parameters", () => {
  it("asks for missing values and focuses the box instead of running", async () => {
    await useStore.getState().runTab("t", "all", input("select * from t where d = :data_date and n = :n"));
    expect(sent).toEqual([]);
    expect(useStore.getState().paramFocus).toMatchObject({ tabId: "t", name: "data_date" });
  });

  it("sends the tab's values with the run; ::casts are not parameters", async () => {
    useStore.getState().setParam("t", "data_date", { value: "2026-09-09" });
    useStore.getState().setParam("t", "n", { value: "2000" });
    await useStore.getState().runTab("t", "all", input("select x::date from t where d = :data_date and n = :n"));
    expect(sent[0].params).toEqual({ data_date: { value: "2026-09-09" }, n: { value: "2000" } });
    useStore.getState().setParam("t", "n", { raw: true });
    expect(useStore.getState().params.t.n).toEqual({ value: "2000", raw: true });
  });

  it("notebook cells use the notebook tab's values", async () => {
    useStore.setState({ tabs: [{ id: "nbtab", title: "N", sql: "", connection_id: "c", notebook_id: "n1" }] });
    expect(await useStore.getState().paramsFor("nbtab", "postgres", "select :d")).toBeNull();
    expect(useStore.getState().paramFocus).toMatchObject({ tabId: "nbtab", name: "d" });
    useStore.getState().setParam("nbtab", "d", { value: "2026-09-09" });
    expect(await useStore.getState().paramsFor("nbtab", "postgres", "select :d")).toEqual({ d: { value: "2026-09-09" } });
    expect(await useStore.getState().paramsFor("nbtab", "postgres", "select 1")).toEqual({ d: { value: "2026-09-09" } });
  });

  it("forgets a closed tab's values", () => {
    useStore.getState().setParam("t", "a", { value: "1" });
    useStore.getState().newTab();
    useStore.getState().closeTab("t");
    expect(useStore.getState().params.t).toBeUndefined();
  });
});
