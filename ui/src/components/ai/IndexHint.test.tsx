import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import type { ConnectionView, DbObject, KnState } from "../../lib/types";
import { useAi } from "../../aiStore";
import { useStore } from "../../store";
import { IndexHint, RETRY_MS } from "./IndexHint";

const conn = (policy: Record<string, unknown> = {}): ConnectionView =>
  ({ id: "c1", name: "Warehouse", config: { kind: "postgres" }, ai_policy: { ai_enabled: true, share_metadata: true, ...policy } }) as unknown as ConnectionView;
const state = (objects: number): KnState => ({ connection_id: "c1", indexed_at: 1, objects, schemas: [], error: null });
const tables = (n: number): DbObject[] => Array.from({ length: n }, (_, i) => ({ schema: "public", name: `t${i}`, kind: "table" }));

beforeEach(() => {
  vi.restoreAllMocks();
  localStorage.clear();
  useAi.setState({ view: "chat", knowledgeFocus: null, knowledgeProgress: {}, knowledgeVersion: 0 });
});

describe("index hint", () => {
  it("offers to index a connection with nothing indexed, opening Knowledge on it", async () => {
    useStore.setState({ connections: [conn()], objects: {} });
    vi.spyOn(api, "knGet").mockResolvedValue({ state: null, notes: [] });
    render(<IndexHint connId="c1" />);
    await screen.findByText(/may not use any of/);
    fireEvent.click(screen.getByText("Choose in Knowledge"));
    expect(useAi.getState().view).toBe("knowledge");
    expect(useAi.getState().knowledgeFocus).toBe("c1");
  });

  it("reads the index state again when a read fails", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      useStore.setState({ connections: [conn()], objects: {} });
      const get = vi.spyOn(api, "knGet").mockRejectedValueOnce(new Error("busy")).mockResolvedValue({ state: null, notes: [] });
      render(<IndexHint connId="c1" />);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(RETRY_MS[0]);
      });
      await screen.findByText(/may not use any of/);
      expect(get).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("says how few tables are indexed compared with the explorer", async () => {
    useStore.setState({ connections: [conn()], objects: { "c1|public": tables(120) } });
    vi.spyOn(api, "knGet").mockResolvedValue({ state: state(10), notes: [] });
    render(<IndexHint connId="c1" />);
    await screen.findByText(/may use 10 of the 120 tables/);
    expect(screen.getByText("Review in Knowledge")).toBeTruthy();
  });

  it("stays away when the index covers the explorer, AI is off, or it was dismissed", async () => {
    useStore.setState({ connections: [conn()], objects: { "c1|public": tables(30) } });
    const get = vi.spyOn(api, "knGet").mockResolvedValue({ state: state(28), notes: [] });
    const { unmount } = render(<IndexHint connId="c1" />);
    await waitFor(() => expect(get).toHaveBeenCalled());
    expect(screen.queryByRole("note")).toBeNull();
    unmount();

    useStore.setState({ connections: [conn({ share_metadata: false })] });
    get.mockResolvedValue({ state: null, notes: [] });
    const r2 = render(<IndexHint connId="c1" />);
    await waitFor(() => expect(get).toHaveBeenCalledTimes(2));
    expect(screen.queryByRole("note")).toBeNull();
    r2.unmount();

    useStore.setState({ connections: [conn()] });
    render(<IndexHint connId="c1" />);
    await screen.findByText(/may not use any of/);
    fireEvent.click(screen.getByLabelText("Dismiss"));
    expect(screen.queryByRole("note")).toBeNull();
  });

  it("doesn't flash the old message when indexing finishes", async () => {
    useStore.setState({ connections: [conn()], objects: { "c1|public": tables(30) } });
    let resolve: (v: { state: KnState; notes: [] }) => void = () => {};
    const get = vi.spyOn(api, "knGet").mockResolvedValueOnce({ state: null, notes: [] });
    render(<IndexHint connId="c1" />);
    await screen.findByText(/may not use any of/);
    get.mockImplementationOnce(() => new Promise((r) => (resolve = r)));
    // Indexing done: progress cleared and the index changed in one update.
    act(() => useAi.setState({ knowledgeProgress: {}, knowledgeVersion: 1 }));
    expect(screen.queryByText(/may not use any of/)).toBeNull();
    await act(async () => resolve({ state: state(30), notes: [] }));
    expect(screen.queryByRole("note")).toBeNull();
  });

  it("shows indexing progress instead", () => {
    useStore.setState({ connections: [conn()], objects: {} });
    vi.spyOn(api, "knGet").mockResolvedValue({ state: null, notes: [] });
    useAi.setState({ knowledgeProgress: { c1: { schema: "sales", done: 2, total: 5 } } });
    render(<IndexHint connId="c1" />);
    expect(screen.getByRole("status").textContent).toContain("Indexing Warehouse: sales (2 of 5)");
  });
});
