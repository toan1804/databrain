import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import type { JobNode, Watermark } from "../../lib/types";
import { useStore } from "../../store";
import { COLUMN_SAVE_DELAY, IncrementalFields, incrementalProblem } from "./IncrementalFields";

const step = (patch: Partial<JobNode> = {}): JobNode => ({ id: "a", name: "orders", kind: "query", sql: "select * from t where id > :watermark", x: 0, y: 0, ...patch }) as JobNode;
const mark: Watermark = { column: "id", value: "42", run_id: 7, updated_at: Date.now() };

describe("incremental step settings", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
    useStore.setState({ confirm: null });
  });

  it("says what's missing", () => {
    expect(incrementalProblem(step(), null)).toBeNull();
    expect(incrementalProblem(step({ incremental_column: "id", incremental_start: "0" }), null)).toBeNull();
    // The first run needs a start value; with a mark it doesn't.
    expect(incrementalProblem(step({ incremental_column: "id" }), null)).toMatch(/Enter a start value/);
    expect(incrementalProblem(step({ incremental_column: "id" }), mark)).toBeNull();
    // SQL that doesn't read the mark reads every row each time.
    expect(incrementalProblem(step({ incremental_column: "id", incremental_start: "0", sql: "select * from t" }), null)).toMatch(/doesn't use :watermark/);
    expect(incrementalProblem(step({ incremental_column: "id", incremental_start: "0", sql: "select * from t where id > :WATERMARK" }), null)).toBeNull();
  });

  it("shows the step's mark and resets it", async () => {
    const get = vi.spyOn(api, "jobWatermark").mockResolvedValue(mark);
    const reset = vi.spyOn(api, "resetJobWatermark").mockResolvedValue(undefined);
    render(<IncrementalFields jobId="j1" node={step({ incremental_column: "id", incremental_start: "0" })} onChange={() => {}} />);
    await screen.findByText("42");
    expect(get).toHaveBeenCalledWith("j1", "a", "id");
    // Reset asks first: the next run reads from the start value again.
    fireEvent.click(screen.getByRole("button", { name: /Reset/ }));
    const ask = useStore.getState().confirm!;
    expect(ask.title).toMatch(/Reset the mark of "orders"/);
    act(() => {
      useStore.getState().askConfirm(null);
      ask.onCancel?.();
    });
    expect(reset).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: /Reset/ }));
    act(() => useStore.getState().confirm!.onConfirm());
    await waitFor(() => expect(reset).toHaveBeenCalledWith("j1", "a"));
    await screen.findByText(/No mark yet/);
  });

  it("a mark looked up before a Reset, answering after it, isn't shown", async () => {
    let answer: (m: Watermark) => void = () => {};
    vi.spyOn(api, "jobWatermark").mockResolvedValueOnce(mark).mockImplementationOnce(() => new Promise((r) => (answer = r)));
    vi.spyOn(api, "resetJobWatermark").mockResolvedValue(undefined);
    const node = step({ incremental_column: "id", incremental_start: "0" });
    const { rerender } = render(<IncrementalFields jobId="j1" node={node} onChange={() => {}} />);
    await screen.findByText("42");
    // A run ends: the mark is looked up again (slow), then the user resets.
    rerender(<IncrementalFields jobId="j1" node={{ ...node, last_run: { finished_at: 5 } as never }} onChange={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: /Reset/ }));
    act(() => useStore.getState().confirm!.onConfirm());
    await screen.findByText(/No mark yet/);
    await act(async () => answer({ ...mark, value: "41" }));
    expect(screen.queryByText("41")).toBeNull();
    expect(screen.getByText(/No mark yet/)).toBeTruthy();
  });

  it("a mark that can't be read says so, not 'no mark yet'", async () => {
    vi.spyOn(api, "jobWatermark").mockRejectedValue({ kind: "io", message: "workspace is locked" });
    render(<IncrementalFields jobId="j1" node={step({ incremental_column: "id", incremental_start: "0" })} onChange={() => {}} />);
    expect((await screen.findByRole("alert")).textContent).toMatch(/couldn't be read: workspace is locked/);
    expect(screen.queryByText(/No mark yet/)).toBeNull();
    // It can still be reset (a mark that can't be read is the one to forget).
    const reset = vi.spyOn(api, "resetJobWatermark").mockResolvedValue(undefined);
    fireEvent.click(screen.getByRole("button", { name: /Reset/ }));
    act(() => useStore.getState().confirm!.onConfirm());
    await waitFor(() => expect(reset).toHaveBeenCalledWith("j1", "a"));
    await screen.findByText(/No mark yet/);
  });

  it("saves the column a moment after typing stops, not per keystroke", () => {
    vi.useFakeTimers();
    const get = vi.spyOn(api, "jobWatermark").mockResolvedValue(null);
    const onChange = vi.fn();
    render(<IncrementalFields jobId="j1" node={step({ incremental_column: "id", incremental_start: "0" })} onChange={onChange} />);
    get.mockClear();
    const input = screen.getByLabelText("Incremental column");
    for (const t of ["u", "up", "upd"]) fireEvent.change(input, { target: { value: t } });
    expect(onChange).not.toHaveBeenCalled();
    act(() => vi.advanceTimersByTime(COLUMN_SAVE_DELAY));
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenLastCalledWith({ incremental_column: "upd" });
    expect(get).not.toHaveBeenCalled();
  });

  it("follows a column cleared from outside, and labels its fields", () => {
    vi.spyOn(api, "jobWatermark").mockResolvedValue(null);
    const node = step({ incremental_column: "id", incremental_start: "0" });
    const { rerender } = render(<IncrementalFields jobId="j1" node={node} onChange={() => {}} />);
    expect((screen.getByRole("checkbox") as HTMLInputElement).checked).toBe(true);
    expect(screen.getByText("Column").getAttribute("for")).toBe(screen.getByLabelText("Incremental column").id);
    rerender(<IncrementalFields jobId="j1" node={{ ...node, incremental_column: undefined }} onChange={() => {}} />);
    expect((screen.getByRole("checkbox") as HTMLInputElement).checked).toBe(false);
  });

  it("turning it off clears the column and start value", () => {
    vi.spyOn(api, "jobWatermark").mockResolvedValue(null);
    const onChange = vi.fn();
    render(<IncrementalFields jobId="j1" node={step({ incremental_column: "id", incremental_start: "0" })} onChange={onChange} />);
    fireEvent.click(screen.getByRole("checkbox"));
    expect(onChange).toHaveBeenCalledWith({ incremental_column: undefined, incremental_start: undefined });
    expect(screen.queryByLabelText("Incremental column")).toBeNull();
  });
});
