import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import type { ConnectionView, ExploreRows, ObjectDetail } from "../lib/types";
import { useStore } from "../store";
import { TableTab } from "./TableTab";
import * as menus from "./CatalogMenus";

const conn = (env: "prod" | "none") =>
  ({
    id: "c1",
    name: "Shop",
    env,
    connected: true,
    has_secret: false,
    created_at: 0,
    updated_at: 0,
    config: { kind: "postgres", auth: { kind: "none" }, ssl_mode: "prefer", read_only: false },
    ai_policy: { ai_enabled: true, share_metadata: true, allowed_providers: [] },
  }) as unknown as ConnectionView;

const detail: ObjectDetail = {
  object: { schema: "public", name: "orders", kind: "table", row_estimate: 3 },
  columns: [
    { name: "id", data_type: "integer", nullable: false, is_primary_key: true },
    { name: "status", data_type: "text", nullable: true, is_primary_key: false },
  ],
  foreign_keys: [],
};

const rows = (columns: string[], ...r: (string | null)[][]): ExploreRows => ({ columns, rows: r, duration_ms: 4 });

/** What each session's read answers (by key), or a promise left open. */
function answer(key: string): Promise<ExploreRows> {
  if (key.endsWith("#profile")) return Promise.resolve(rows(["total", "n0", "d0", "lo0", "hi0", "n1", "d1"], ["3", "3", "3", "1", "3", "2", "2"]));
  if (key.endsWith("#top:status")) return Promise.resolve(rows(["value", "n"], ["paid", "2"], [null, "1"]));
  if (key.endsWith("#partitions")) return Promise.resolve(rows(["name", "bound", "row_count", "bytes"], ["orders_2026", "FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')", "1200", "2048"]));
  if (key.endsWith("#permissions")) return Promise.resolve(rows(["grantee", "privilege", "grantable", "grantor", "scope"], ["analyst", "SELECT", "no", "owner", "table"]));
  if (key.endsWith("#stats")) return Promise.resolve(rows(["row_estimate", "total_bytes", "last_analyzed"], ["3", "16384", null]));
  return new Promise(() => {});
}

describe("table explorer: profile, top values, statistics", () => {
  let confirm: ReturnType<typeof vi.fn>;
  let closed: string[];
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    closed = [];
    confirm = vi.fn((o: { onConfirm: () => void }) => o.onConfirm());
    vi.spyOn(api, "describeObject").mockResolvedValue(detail);
    vi.spyOn(api, "knTableNotes").mockResolvedValue({ target: "public.orders", notes: [] });
    vi.spyOn(api, "exploreQuery").mockImplementation((_c, key) => answer(key));
    vi.spyOn(api, "closeTab").mockImplementation(async (key: string) => void closed.push(key));
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  const open = (env: "prod" | "none") => {
    useStore.setState({ connections: [conn(env)], tabs: [{ id: "t1", title: "orders", sql: "", connection_id: "c1", table: { schema: "public", name: "orders", kind: "table" } } as never], askConfirm: confirm as never });
    return render(<TableTab tabId="t1" reference={{ schema: "public", name: "orders", kind: "table" }} visible />);
  };

  it("copies the table's path and its name from the header", async () => {
    const copy = vi.spyOn(menus, "copyText").mockResolvedValue();
    open("none");
    fireEvent.click(await screen.findByRole("button", { name: "Copy path" }));
    expect(copy).toHaveBeenLastCalledWith("public.orders", "table path");
    fireEvent.click(screen.getByRole("button", { name: "Copy name" }));
    expect(copy).toHaveBeenLastCalledWith("orders", "table name");
  });

  it("asks once on production, profiles every column, and stops a column's top values when another opens", async () => {
    open("prod");
    fireEvent.click(await screen.findByRole("tab", { name: /^Columns/ }));
    fireEvent.click(await screen.findByRole("button", { name: /Profile columns/ }));
    expect(await screen.findByText("3 rows read")).toBeTruthy();
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(vi.mocked(api.exploreQuery).mock.calls[0][1]).toBe("t1#profile");

    // Top values of status: its own session, not asked again in this tab.
    const distinct = screen.getAllByRole("button", { name: "2" }).find((b) => b.getAttribute("aria-expanded") != null)!;
    fireEvent.click(distinct);
    expect(await screen.findByText("paid")).toBeTruthy();
    expect(confirm).toHaveBeenCalledTimes(1);

    // Opening id's closes status's session (which stops it), and id's reads on its own.
    fireEvent.click(screen.getAllByRole("button", { name: "3" }).find((b) => b.getAttribute("aria-expanded") != null)!);
    await act(async () => void vi.advanceTimersByTime(1));
    expect(closed).toContain("t1#top:status");
    expect(vi.mocked(api.exploreQuery).mock.calls.map((c) => c[1])).toContain("t1#top:id");
  });

  it("reads the catalog's statistics when the part opens, and Stop counting closes the count's session", async () => {
    open("none");
    fireEvent.click(await screen.findByRole("tab", { name: /^Statistics/ }));
    expect(await screen.findByText("Rows (the catalog's figure)")).toBeTruthy();
    expect(screen.getByText("16 KB")).toBeTruthy();
    expect(screen.queryByText("Last analyzed")).toBeNull();
    // Nothing asked: a catalog read, and a small table on a connection that isn't production.
    fireEvent.click(screen.getByRole("button", { name: /Count exactly/ }));
    fireEvent.click(await screen.findByRole("button", { name: /Stop counting/ }));
    expect(confirm).not.toHaveBeenCalled();
    await waitFor(() => expect(closed).toContain("t1#count"));
  });

  it("opens on Columns without reading rows, with the description in full", async () => {
    const run = vi.spyOn(useStore.getState(), "runSql");
    vi.mocked(api.describeObject).mockResolvedValue({ ...detail, object: { ...detail.object, comment: "line one\nline two\nline three\nline four" } });
    open("none");
    expect((await screen.findByRole("tab", { name: /^Columns/ })).getAttribute("aria-selected")).toBe("true");
    expect(await screen.findByText("status")).toBeTruthy();
    // Every line is there (three show, the rest scroll), not cut to one.
    expect(screen.getByLabelText("Description").textContent).toBe("line one\nline two\nline three\nline four");
    expect(screen.getByRole("button", { name: "Copy the description" })).toBeTruthy();
    expect(run).not.toHaveBeenCalled();
    expect(screen.getAllByRole("tab").map((t) => t.textContent?.replace(/\d+$/, ""))).toEqual(["Columns", "Data", "Statistics", "Relations", "Usage", "Indexes", "Partitions", "Permissions", "History", "DDL", "Notes"]);
  });

  it("shows no description when the table has none", async () => {
    vi.mocked(api.describeObject).mockResolvedValue({ ...detail, object: { ...detail.object, comment: "  " } });
    open("none");
    expect(await screen.findByText("status")).toBeTruthy();
    expect(screen.queryByLabelText("Description")).toBeNull();
  });

  it("reads the partitions and permissions when their parts open", async () => {
    open("none");
    fireEvent.click(await screen.findByRole("tab", { name: "Partitions" }));
    expect(await screen.findByText("orders_2026")).toBeTruthy();
    expect(screen.getByText("1,200")).toBeTruthy();
    expect(screen.getByText("1 partition")).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "Permissions" }));
    expect(await screen.findByText("analyst")).toBeTruthy();
    expect(screen.getByText("Can grant")).toBeTruthy();
    expect(confirm).not.toHaveBeenCalled();
  });
});
