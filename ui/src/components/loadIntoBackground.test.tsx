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
const report: LoadReport = { rows: 1000, deleted: 0, created_table: false, transaction: true, notices: [] };
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
