import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Progress comes from the test, not Tauri.
let progress: ((e: { load_id: string; done: number; total: number }) => void) | null = null;
vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  onLoadProgress: (h: typeof progress) => ((progress = h), Promise.resolve(() => {})),
}));

const { api } = await import("../lib/api");
const { useStore } = await import("../store");
const { useLoads } = await import("../loadsStore");
const { useActivity } = await import("../activityStore");
const { LoadIntoDialog } = await import("./LoadIntoDialog");
const { ActivityBar } = await import("./ActivityBar");
import type { LoadReport, ResultInfo } from "../lib/types";

const info = { id: "r1", total_rows: 1000, truncated: false, columns: [] } as unknown as ResultInfo;
const report: LoadReport = { rows: 1000, deleted: 0, created_table: false, transaction: true, notices: [], warnings: [] };
const target = { resolved: "orders", catalog: null, schema: null, table: "orders", catalog_exists: null, schema_exists: null, table_exists: true, problems: [], notes: [], respelled: false };

/** The dialog open on "select * from orders" with Warehouse picked, ready to load. */
async function openDialog(onClose = vi.fn()) {
  const view = render(
    <>
      <LoadIntoDialog info={info} sql="select * from orders" onClose={onClose} />
      <ActivityBar />
    </>,
  );
  fireEvent.change(screen.getByRole("combobox", { name: /Into connection/ }), { target: { value: "c1" } });
  await waitFor(() => expect(screen.getByRole("button", { name: /Load 1,000 rows/ })).not.toHaveProperty("disabled", true));
  return { view, onClose };
}

const bar = () => screen.queryByRole("region", { name: "Background activity" });

describe("Load into… in the background", () => {
  let finish: (r: LoadReport) => void;
  let failWith: (e: unknown) => void;
  beforeEach(() => {
    useStore.setState({ connections: [{ id: "c1", name: "Warehouse", env: "dev", config: { kind: "postgres", read_only: false } } as never], toasts: [] });
    useLoads.setState({ items: [], inDialog: null });
    useActivity.setState({ hidden: {} });
    vi.spyOn(api, "checkLoadTarget").mockResolvedValue(target as never);
    vi.spyOn(api, "loadResult").mockImplementation(() => new Promise<LoadReport>((res, rej) => ((finish = res), (failWith = rej))));
  });
  afterEach(() => vi.restoreAllMocks());

  it("shows the load in its dialog, then in the bar once the dialog is left, and how it ended", async () => {
    const { onClose } = await openDialog();
    act(() => screen.getByRole("button", { name: /Load 1,000 rows/ }).click());
    await waitFor(() => expect(screen.getByRole("button", { name: "Continue in background" })).toBeTruthy());
    expect(bar()).toBeNull(); // the dialog shows it
    const id = useLoads.getState().items[0].id;
    act(() => progress!({ load_id: id, done: 250, total: 1000 }));
    expect(screen.getByText(/Writing 250 of 1,000 rows/)).toBeTruthy();

    act(() => screen.getByRole("button", { name: "Continue in background" }).click());
    expect(onClose).toHaveBeenCalled();
    expect(bar()!.textContent).toContain("Load into orders on Warehouse · 250 / 1,000 rows · 25%");

    await act(async () => finish(report));
    expect(screen.getByRole("status", { name: "Load into orders" }).textContent).toContain("Loaded 1,000 rows into orders");
  });

  it("keeps a failure in the bar until dismissed; Stop stops it", async () => {
    await openDialog();
    act(() => screen.getByRole("button", { name: /Load 1,000 rows/ }).click());
    await waitFor(() => expect(screen.getByRole("button", { name: "Continue in background" })).toBeTruthy());
    act(() => screen.getByRole("button", { name: "Continue in background" }).click());
    const cancel = vi.spyOn(api, "cancelLoad").mockResolvedValue(true);
    act(() => screen.getByRole("button", { name: /Load into orders/ }).click());
    act(() => within(screen.getByRole("dialog", { name: "Background work" })).getByRole("button", { name: /Stop/ }).click());
    expect(cancel).toHaveBeenCalledWith(useLoads.getState().items[0].id);
    await act(async () => failWith({ kind: "query", message: "permission denied for table orders" }));
    expect(screen.getByRole("alert", { name: "Load into orders" }).textContent).toContain("permission denied for table orders");
    act(() => screen.getByRole("button", { name: "Dismiss Load into orders" }).click());
    expect(bar()).toBeNull();
  });

  it("a load hidden from the bar that ends well is a toast", async () => {
    await openDialog();
    act(() => screen.getByRole("button", { name: /Load 1,000 rows/ }).click());
    await waitFor(() => expect(screen.getByRole("button", { name: "Continue in background" })).toBeTruthy());
    act(() => screen.getByRole("button", { name: "Continue in background" }).click());
    act(() => screen.getByRole("button", { name: "Hide the activity bar" }).click());
    expect(bar()).toBeNull();
    await act(async () => finish(report));
    expect(bar()).toBeNull();
    expect(useStore.getState().toasts.at(-1)?.message).toBe("Loaded 1,000 rows into orders");
  });

  it("a load asked again for production after it went to the background stays in the bar, and its end shows", async () => {
    const calls: { res: (r: LoadReport) => void; rej: (e: unknown) => void }[] = [];
    vi.mocked(api.loadResult).mockImplementation(() => new Promise<LoadReport>((res, rej) => void calls.push({ res, rej })));
    await openDialog();
    act(() => screen.getByRole("button", { name: /Load 1,000 rows/ }).click());
    await waitFor(() => expect(screen.getByRole("button", { name: "Continue in background" })).toBeTruthy());
    act(() => screen.getByRole("button", { name: "Continue in background" }).click());
    // The server asks: production. The user says yes; it starts again.
    await act(async () => calls[0].rej({ kind: "invalid", message: '"Warehouse" is a production connection', code: "needs_confirmation" }));
    await waitFor(() => expect(useStore.getState().confirm).not.toBeNull());
    const c = useStore.getState().confirm!;
    act(() => {
      useStore.getState().askConfirm(null);
      c.onConfirm();
    });
    await waitFor(() => expect(calls).toHaveLength(2));
    expect(useLoads.getState().inDialog).toBeNull();
    expect(bar()!.textContent).toContain("Load into orders on Warehouse");
    await act(async () => calls[1].res(report));
    expect(screen.getByRole("status", { name: "Load into orders" }).textContent).toContain("Loaded 1,000 rows into orders");
  });

  it("a load that ends while its dialog shows it stays there, with its report", async () => {
    await openDialog();
    act(() => screen.getByRole("button", { name: /Load 1,000 rows/ }).click());
    await waitFor(() => expect(screen.getByRole("button", { name: "Continue in background" })).toBeTruthy());
    await act(async () => finish(report));
    expect(screen.getByText(/rows written into/).textContent).toContain("1,000 rows written into orders on Warehouse");
    expect(bar()).toBeNull();
    expect(useLoads.getState().items).toHaveLength(0);
  });
});

describe("closing a tab that background work runs on", () => {
  beforeEach(() => {
    useStore.setState({ tabs: [{ id: "t1", title: "Orders", sql: "", connection_id: null } as never], activeTabId: "t1", confirm: null, toasts: [], txOpen: {} });
    useLoads.setState({ items: [], inDialog: null });
  });

  it("asks first, and the load says why it stopped", () => {
    vi.spyOn(api, "cancelLoad").mockResolvedValue(true);
    vi.spyOn(api, "closeTab").mockResolvedValue(undefined as never);
    useLoads.getState().begin({ id: "l1", table: "orders", target: "Warehouse", total: 0, tabId: "t1" });
    useStore.getState().requestCloseTab("t1");
    const c = useStore.getState().confirm!;
    expect(c.reasons).toEqual(["Closing it stops what runs on its connection session: the load into orders."]);
    expect(c.confirmLabel).toBe("Close and stop");
    expect(useStore.getState().tabs.map((t) => t.id)).toEqual(["t1"]);
    act(() => {
      useStore.getState().askConfirm(null);
      c.onConfirm();
    });
    expect(useStore.getState().tabs.some((t) => t.id === "t1")).toBe(false);
    // The backend stops it (cancelled): the outcome says why.
    useLoads.getState().stopped("l1");
    expect(useStore.getState().toasts.at(-1)?.message).toBe("Load into orders stopped: its tab was closed");
  });

  it("doesn't ask for a load that reads the rows the result holds (no tab session)", () => {
    vi.spyOn(api, "closeTab").mockResolvedValue(undefined as never);
    useLoads.getState().begin({ id: "l2", table: "orders", target: "Warehouse", total: 10 });
    useStore.getState().requestCloseTab("t1");
    expect(useStore.getState().confirm).toBeNull();
    expect(useStore.getState().tabs.some((t) => t.id === "t1")).toBe(false);
  });
});

describe("Stop while test data rows are still being made", () => {
  afterEach(() => vi.restoreAllMocks());

  it("cancels the AI request making them; once loading, the load", () => {
    const ai = vi.spyOn(api, "aiCancel").mockResolvedValue(true as never);
    const load = vi.spyOn(api, "cancelLoad").mockResolvedValue(true);
    useLoads.setState({ items: [], inDialog: null });
    const what = { id: "t1", table: "people", target: "Dev", label: "Test data into people" };
    useLoads.getState().begin({ ...what, total: 100, phase: "AI is writing 100 rows…", aiRun: "run-1" });
    useLoads.getState().cancel("t1");
    expect(ai).toHaveBeenCalledWith("run-1");
    expect(load).not.toHaveBeenCalled();
    useLoads.getState().begin({ ...what, total: 100 });
    expect(useLoads.getState().items[0].phase).toBeUndefined();
    useLoads.getState().cancel("t1");
    expect(load).toHaveBeenCalledWith("t1");
  });
});
