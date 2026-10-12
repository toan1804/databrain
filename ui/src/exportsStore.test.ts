import { beforeEach, describe, expect, it, vi } from "vitest";

// Backend stub: each export resolves/rejects when the test says so.
const pending = new Map<string, { resolve: (n: number) => void; reject: (e: unknown) => void }>();
let progress: ((e: { export_id: string; phase: string; rows: number; bytes: number }) => void) | null = null;
const cancelled: string[] = [];
vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => (typeof e === "object" && e && "kind" in e ? e : { kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  onOracleAgent: async () => () => {},
  onExportProgress: async (h: typeof progress) => {
    progress = h;
    return () => {};
  },
  api: {
    exportFullResult: (_r: string, _o: unknown, _p: string, id: string) => new Promise((resolve, reject) => pending.set(id, { resolve, reject })),
    cancelExport: async (id: string) => (cancelled.push(id), true),
    revealExportFile: async () => {},
  },
}));

const { useExports } = await import("./exportsStore");
const { useStore } = await import("./store");
const opts = { format: "csv", header: true, table_name: "t", dialect: null } as never;
const flush = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => {
  useExports.setState({ items: [], inDialog: null });
  useStore.setState({ toasts: [] });
  pending.clear();
  cancelled.length = 0;
});

describe("background exports", () => {
  it("tracks progress while running and keeps the finished export with its rows", async () => {
    const id = await useExports.getState().start({ resultId: "r1", options: opts, path: "/tmp/out/all.csv", source: "Sales" });
    let x = useExports.getState().items[0];
    expect(x).toMatchObject({ id, fileName: "all.csv", status: "running", phase: "connecting" });
    progress!({ export_id: id, phase: "writing", rows: 20000, bytes: 4096 });
    progress!({ export_id: "other", phase: "writing", rows: 1, bytes: 1 });
    x = useExports.getState().items[0];
    expect(x).toMatchObject({ phase: "writing", rows: 20000, bytes: 4096 });
    pending.get(id)!.resolve(50000);
    await flush();
    x = useExports.getState().items[0];
    expect(x).toMatchObject({ status: "done", rows: 50000 });
    // Late progress doesn't change a finished export.
    progress!({ export_id: id, phase: "writing", rows: 1, bytes: 1 });
    expect(useExports.getState().items[0].rows).toBe(50000);
  });

  it("several exports run at once; Stop removes the stopped one", async () => {
    const a = await useExports.getState().start({ resultId: "r1", options: opts, path: "/a.csv", source: "A" });
    const b = await useExports.getState().start({ resultId: "r2", options: opts, path: "/b.csv", source: "B" });
    expect(useExports.getState().items).toHaveLength(2);
    useExports.getState().cancel(a);
    expect(cancelled).toEqual([a]);
    expect(useExports.getState().items.find((x) => x.id === a)?.stopping).toBe(true);
    pending.get(a)!.reject({ kind: "cancelled", message: "cancelled" });
    await flush();
    expect(useExports.getState().items.map((x) => x.id)).toEqual([b]);
    expect(useStore.getState().toasts.at(-1)?.message).toContain("a.csv stopped");
  });

  it("keeps a failed export with its error until dismissed", async () => {
    const id = await useExports.getState().start({ resultId: "r1", options: opts, path: "/x.csv", source: "X" });
    pending.get(id)!.reject({ kind: "query", message: "relation does not exist" });
    await flush();
    expect(useExports.getState().items[0]).toMatchObject({ status: "failed", error: "relation does not exist" });
    useExports.getState().dismiss(id);
    expect(useExports.getState().items).toHaveLength(0);
  });
});
