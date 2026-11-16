import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { Job, JobNode } from "../../lib/types";
import { newNode } from "../../lib/jobGraph";
import { JobLimits, StepLimitFields, durationText, parseCount, parseDuration } from "./LimitFields";
import { applyPatch, statusText } from "./shared";

const job = (limits?: Job["limits"]): Job => ({ id: "j1", name: "Job", nodes: [], edges: [], schedule: { enabled: false } as never, created_at: 0, updated_at: 0, limits }) as never;

describe("job limits", () => {
  it("reads durations as typed", () => {
    expect(parseDuration("")).toBeNull();
    expect(parseDuration("30")).toBe(1800); // a plain number is minutes
    expect(parseDuration("1.5")).toBe(90);
    expect(parseDuration("90s")).toBe(90);
    expect(parseDuration("30m")).toBe(1800);
    expect(parseDuration("30 min")).toBe(1800);
    expect(parseDuration("2h")).toBe(7200);
    expect(parseDuration("1h 30m")).toBe(5400);
    for (const bad of ["0", "0s", "-5", "abc", "5d", "h", "1.2.3"]) expect(parseDuration(bad), bad).toBe("invalid");
    expect(durationText(1800)).toBe("30m");
    expect(durationText(5400)).toBe("1h30m");
    expect(durationText(90)).toBe("1m30s");
    expect(durationText(null)).toBe("");
    // What is shown reads back the same.
    for (const s of [45, 60, 3600, 3725]) expect(parseDuration(durationText(s))).toBe(s);
  });

  it("reads counts within their range", () => {
    expect(parseCount("", 1)).toBeNull();
    expect(parseCount("3", 1)).toBe(3);
    expect(parseCount("0", 0)).toBe(0);
    expect(parseCount("0", 1)).toBe("invalid");
    expect(parseCount("2.5", 1)).toBe("invalid");
    expect(parseCount("11", 0, 10)).toBe("invalid");
  });

  it("saves a valid value when the field is left, keeps an invalid one to correct, unsaved", () => {
    const onChange = vi.fn();
    render(<JobLimits job={job({ retries: 2 })} onChange={onChange} />);
    const timeout = screen.getByLabelText("Run time limit");
    fireEvent.change(timeout, { target: { value: "1h" } });
    fireEvent.blur(timeout);
    expect(onChange).toHaveBeenLastCalledWith({ limits: { retries: 2, run_timeout_secs: 3600 } });

    onChange.mockClear();
    const parallel = screen.getByLabelText("Steps at once");
    fireEvent.change(parallel, { target: { value: "0" } });
    fireEvent.blur(parallel);
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByText("A whole number from 1 to 64")).toBeTruthy();
    // The typo stays, to be corrected, and the field says what's wrong.
    expect((parallel as HTMLInputElement).value).toBe("0");
    expect(parallel.getAttribute("aria-invalid")).toBe("true");
    expect(screen.getByText("A whole number from 1 to 64").id).toBe(parallel.getAttribute("aria-describedby"));

    // Emptied: the job's limits are left out, not saved as null.
    const retries = screen.getByLabelText("Retries after a connection error");
    fireEvent.change(retries, { target: { value: "" } });
    fireEvent.keyDown(retries, { key: "Enter" });
    expect(onChange).toHaveBeenLastCalledWith({ limits: undefined });
  });

  it("saves a typed value when the panel closes before the field is left", () => {
    const onChange = vi.fn();
    const { unmount } = render(<JobLimits job={job({})} onChange={onChange} />);
    fireEvent.change(screen.getByLabelText("Run time limit"), { target: { value: "2h" } });
    // A typo is not saved on close either.
    fireEvent.change(screen.getByLabelText("Steps at once"), { target: { value: "0" } });
    unmount();
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(applyPatch(job({}), onChange.mock.lastCall![0]).limits).toEqual({ run_timeout_secs: 7200 });
  });

  it("on close, keeps a change made elsewhere meanwhile", () => {
    const onChange = vi.fn();
    const { unmount } = render(<JobLimits job={job({ retries: 1 })} onChange={onChange} />);
    fireEvent.change(screen.getByLabelText("Run time limit"), { target: { value: "2h" } });
    unmount();
    const patch = onChange.mock.lastCall![0];
    // The assistant set retries meanwhile: kept, the typed limit added to it.
    expect(applyPatch(job({ retries: 3 }), patch).limits).toEqual({ retries: 3, run_timeout_secs: 7200 });
    // It set the run limit itself meanwhile: the typed one doesn't overwrite it.
    expect(applyPatch(job({ run_timeout_secs: 60 }), patch).limits).toEqual({ run_timeout_secs: 60 });
  });

  it("a step's own limits: empty is the job's, 0 none", () => {
    const onChange = vi.fn();
    const j = job({ step_timeout_secs: 600, retries: 3 });
    const node: JobNode = newNode(j, { x: 0, y: 0, name: "orders" });
    render(<StepLimitFields job={j} node={node} onChange={onChange} />);
    const timeout = screen.getByLabelText("Time limit") as HTMLInputElement;
    expect(timeout.placeholder).toBe("job's: 10m");
    expect((screen.getByLabelText("Retries") as HTMLInputElement).placeholder).toBe("job's: 3");
    fireEvent.change(timeout, { target: { value: "0" } });
    fireEvent.blur(timeout);
    expect(onChange).toHaveBeenLastCalledWith({ limits: { timeout_secs: 0 } });
  });

  it("step status says which try and why it waits", () => {
    const base = { finished_at: 0, duration_ms: 0 };
    expect(statusText({ ...base, status: "running", attempt: 2, attempts: 3 })).toBe("Running… (attempt 2 of 3)");
    expect(statusText({ ...base, status: "running", attempt: 1, attempts: 3 })).toBe("Running…");
    expect(statusText({ ...base, status: "error", error: "lost connection", attempt: 3, attempts: 3 })).toBe("lost connection (after 3 tries)");
    expect(statusText({ ...base, status: "waiting", error: "2 steps are running on Sales (the job's limit per connection)" })).toBe(
      "Waiting: 2 steps are running on Sales (the job's limit per connection)",
    );
    expect(statusText({ ...base, status: "waiting" })).toBe("Waiting for a free slot");
  });
});
