import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { HistoryEntry, HistoryQuery } from "../../lib/types";
import { api } from "../../lib/api";
import { useStore } from "../../store";
import { HistoryPanel } from "./HistoryPanel";

const entry: HistoryEntry = {
  id: 7,
  connection_id: "pg",
  connection_name: "Sales",
  sql: "select count(*) from orders",
  started_at: Date.now(),
  duration_ms: 12,
  rows: 1,
  status: "success",
  error: null,
  origin: "user",
  output_handle: null,
  result_id: null,
};
const queries: HistoryQuery[] = [];

beforeEach(() => {
  queries.length = 0;
  vi.spyOn(api, "listHistory").mockImplementation(async (q) => {
    queries.push(q);
    return [entry];
  });
  const conn = (id: string, name: string) => ({ id, name, connected: false, config: { kind: "postgres", auth: { method: "none" }, options: {} } });
  useStore.setState({ connections: [conn("pg", "Sales"), conn("pg2", "Sales copy")] as never, tabs: [] });
});

describe("History panel", () => {
  it("sends the connection and status filters with the search", async () => {
    const user = userEvent.setup();
    render(<HistoryPanel />);
    await screen.findByText(/select count/);
    await user.selectOptions(screen.getByRole("combobox", { name: "Connection" }), "pg");
    await user.selectOptions(screen.getByRole("combobox", { name: "Status" }), "error");
    await waitFor(() => expect(queries.at(-1)).toMatchObject({ connection_id: "pg", status: "error" }));
  });

  it("runs an entry again in a new tab, on its connection or another one", async () => {
    const user = userEvent.setup();
    const runTab = vi.fn(async () => {});
    useStore.setState({ runTab });
    render(<HistoryPanel />);
    await user.click(await screen.findByRole("button", { name: "Run again" }));
    let tab = useStore.getState().tabs.at(-1) as { sql: string; connection_id: string };
    expect([tab.sql, tab.connection_id]).toEqual([entry.sql, "pg"]);
    expect(runTab).toHaveBeenCalledWith(expect.any(String), "all", expect.objectContaining({ doc: entry.sql }));

    await user.pointer({ keys: "[MouseRight]", target: screen.getByText(/select count/) });
    await user.click(screen.getByRole("button", { name: "Sales copy" }));
    tab = useStore.getState().tabs.at(-1) as { sql: string; connection_id: string };
    expect(tab.connection_id).toBe("pg2");
    expect(runTab).toHaveBeenCalledTimes(2);
  });
});
