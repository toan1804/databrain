import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import type { ConnectionView, Job } from "../../lib/types";
import { useStore } from "../../store";
import { newNode } from "../../lib/jobGraph";
import { NodePanel } from "./NodePanel";

// jsdom has no layout; the panel's SQL editor measures text ranges.
Range.prototype.getClientRects = () => Object.assign([], { item: () => null }) as unknown as DOMRectList;
Range.prototype.getBoundingClientRect = () => new DOMRect();

const emptyJob = (): Job => ({ id: "j1", name: "Job", nodes: [], edges: [], schedule: { enabled: false } as never, created_at: 0, updated_at: 0 }) as never;
const conn = (id: string, name: string, kind = "postgres"): ConnectionView => ({ id, name, env: "none", config: { kind, read_only: false, auth: { method: "none" }, options: {} } }) as never;

function panel(connectionId: string | null, onChange = vi.fn()) {
  const job = emptyJob();
  const node = { ...newNode(job, { x: 0, y: 0, name: "orders" }), connection_id: connectionId };
  job.nodes = [node];
  render(<NodePanel job={job} taken={new Map()} node={node} status={null} running={false} onChange={onChange} onRun={() => {}} onSelect={() => {}} onDelete={() => {}} />);
  return { node, onChange };
}

describe("job step panel", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    useStore.setState({ connections: [] });
  });

  it("keeps a new name when the field is left at once (no wait for the check's pause)", async () => {
    vi.spyOn(api, "outputNameUser").mockResolvedValue(null);
    useStore.setState({ connections: [conn("c1", "Sales")] });
    const { onChange } = panel("c1");
    const input = screen.getByLabelText("Step (output name)");
    fireEvent.change(input, { target: { value: "orders_eu" } });
    fireEvent.blur(input);
    await waitFor(() => expect(onChange).toHaveBeenCalledWith({ name: "orders_eu" }));
  });

  it("doesn't rename to a name another output uses", async () => {
    vi.spyOn(api, "outputNameUser").mockResolvedValue("job Other");
    useStore.setState({ connections: [conn("c1", "Sales")] });
    const { onChange } = panel("c1");
    const input = screen.getByLabelText("Step (output name)");
    fireEvent.change(input, { target: { value: "taken_name" } });
    fireEvent.blur(input);
    await new Promise((r) => setTimeout(r, 20));
    expect(onChange).not.toHaveBeenCalledWith({ name: "taken_name" });
  });

  it("says when its connection was deleted, instead of showing Results", () => {
    useStore.setState({ connections: [conn("c1", "Sales")] });
    panel("gone");
    const select = screen.getByLabelText("Connection") as HTMLSelectElement;
    expect(select.value).toBe("__gone");
    expect(screen.getByText("Deleted connection: choose one")).toBeTruthy();
  });

  it("says when the SQL reads today's date, and what to use instead", () => {
    useStore.setState({ connections: [conn("c1", "Sales")] });
    const job = emptyJob();
    const node = { ...newNode(job, { x: 0, y: 0, name: "orders" }), connection_id: "c1", sql: "select * from orders where day = current_date - 1" };
    job.nodes = [node];
    const { rerender } = render(<NodePanel job={job} taken={new Map()} node={node} status={null} running={false} onChange={() => {}} onRun={() => {}} onSelect={() => {}} onDelete={() => {}} />);
    expect(screen.getByRole("note")).toHaveTextContent(/Uses current_date: .* Use :run_date/);
    const fixed = { ...node, sql: "select * from orders where day = :run_date - 1" };
    rerender(<NodePanel job={{ ...job, nodes: [fixed] }} taken={new Map()} node={fixed} status={null} running={false} onChange={() => {}} onRun={() => {}} onSelect={() => {}} onDelete={() => {}} />);
    expect(screen.queryByRole("note")).not.toBeInTheDocument();
  });

  it("shows the connection it runs on", () => {
    useStore.setState({ connections: [conn("c1", "Sales")] });
    panel("c1");
    expect((screen.getByLabelText("Connection") as HTMLSelectElement).value).toBe("c1");
  });
});
