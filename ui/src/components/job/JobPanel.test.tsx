import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import { cronProblem, timeZoneProblem } from "../../lib/jobGraph";
import type { Job, ParamSpan } from "../../lib/types";
import { useStore } from "../../store";
import { JobPanel } from "./JobPanel";

const job = (sql: string): Job =>
  ({
    id: "j1",
    name: "Daily sales",
    nodes: [{ id: "n1", name: "sales", kind: "query", sql, x: 0, y: 0, load_mode: "append", connection_id: null }],
    edges: [],
    schedule: { enabled: false, mode: "daily", minutes: 60, at: "06:00", weekdays: [] },
    created_at: 0,
    updated_at: 0,
  }) as Job;
const spans = (...names: string[]) => names.map((name) => ({ name, start: 0, end: 0 })) as unknown as ParamSpan[];

describe("job parameters", () => {
  beforeEach(() => {
    vi.spyOn(api, "jobRuns").mockResolvedValue([]);
    vi.spyOn(api, "checkJobCron").mockImplementation(async (c) => cronProblem(c));
    vi.spyOn(api, "checkJobTimeZone").mockImplementation(async (z) => timeZoneProblem(z));
    vi.spyOn(api, "jobTimeZones").mockResolvedValue(["UTC"]);
    useStore.setState({ paramSets: [], loadParamSets: async () => {} } as never);
  });
  afterEach(() => vi.restoreAllMocks());

  it("fills :run_date itself, and runs for a date picked", async () => {
    vi.spyOn(api, "sqlParameters").mockResolvedValue(spans("run_date", "tenant"));
    const onRunWith = vi.fn();
    render(<JobPanel job={job("select * from sales where day = :run_date - 1 and tenant = :tenant")} canResume={false} onResume={() => {}} onRunWith={onRunWith} onChange={() => {}} onSelect={() => {}} />);
    // :tenant needs a value; :run_date is explained, not asked for.
    await waitFor(() => expect(screen.getByLabelText("Run date")).toHaveTextContent("the date (and time) the run is for"), { timeout: 2000 });
    expect(screen.getByText(":tenant")).toBeInTheDocument();
    expect(screen.queryByText(":run_date", { selector: "label" })).not.toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("Date to run the job for"), { target: { value: "2026-11-16" } });
    fireEvent.click(screen.getByRole("button", { name: /Run for this date/ }));
    expect(onRunWith).toHaveBeenCalledWith({ values: { run_date: { value: "2026-11-16" } } });
  });

  it("shows nothing about it for a job that doesn't use it", async () => {
    vi.spyOn(api, "sqlParameters").mockResolvedValue(spans("tenant"));
    render(<JobPanel job={job("select * from sales where tenant = :tenant")} canResume={false} onResume={() => {}} onRunWith={() => {}} onChange={() => {}} onSelect={() => {}} />);
    await waitFor(() => expect(screen.getByText(":tenant")).toBeInTheDocument(), { timeout: 2000 });
    expect(screen.queryByLabelText("Run date")).not.toBeInTheDocument();
  });
});
