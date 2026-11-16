import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { NodeCard } from "./NodeCard";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import { cronProblem, timeZoneProblem } from "../../lib/jobGraph";
import { applyPatch } from "./shared";
import type { ConnectionView, Job, JobSchedule, JobSummary } from "../../lib/types";
import { useStore } from "../../store";
import { useJobs } from "../../jobsStore";
import { MAX_MINUTES, ScheduleSection, parseMinutes } from "./ScheduleSection";

const schedule = (patch: Partial<JobSchedule> = {}): JobSchedule => ({ enabled: true, mode: "interval", minutes: 60, at: "08:00", weekdays: [], ...patch });
const job = (patch: Partial<JobSchedule> = {}, connectionId = "c1"): Job =>
  ({ id: "j1", name: "Nightly", nodes: [{ id: "n1", name: "orders", kind: "query", sql: "", x: 0, y: 0, load_mode: "append", connection_id: connectionId }], edges: [], schedule: schedule(patch), created_at: 0, updated_at: 0 }) as Job;
const conn = (id: string, name: string, method: string): ConnectionView => ({ id, name, env: "none", config: { kind: "snowflake", read_only: false, auth: { method }, options: {} } }) as never;
const summary = (id: string, name: string, after: string[] = []): JobSummary => ({ id, name, node_count: 1, step_names: [], schedule: schedule({ enabled: false, after_jobs: after }), updated_at: 0 }) as JobSummary;

function show(j: Job, nextRunAt: number | null = null) {
  const onChange = vi.fn();
  const r = render(<ScheduleSection job={j} nextRunAt={nextRunAt} onChange={onChange} />);
  return Object.assign(onChange, { unmount: r.unmount, rerender: (k: Job) => r.rerender(<ScheduleSection job={k} nextRunAt={nextRunAt} onChange={onChange} />) });
}

/** The schedule `j` has once the last change was applied to it. */
const applied = (onChange: ReturnType<typeof vi.fn>, j: Job) => applyPatch(j, onChange.mock.lastCall![0]).schedule;

describe("job schedule editor", () => {
  // The backend's checks, stood in for by the same rules (cases where they differ mock their own).
  beforeEach(() => {
    vi.spyOn(api, "checkJobCron").mockImplementation(async (c) => cronProblem(c));
    vi.spyOn(api, "checkJobTimeZone").mockImplementation(async (z) => timeZoneProblem(z));
    vi.spyOn(api, "jobTimeZones").mockResolvedValue(["UTC", "Europe/Paris"]);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    useStore.setState({ connections: [] });
    useJobs.setState({ jobs: [] });
  });

  it("switches kind, adds a time of day, picks month days", () => {
    let onChange = show(job());
    fireEvent.click(screen.getByRole("button", { name: "Monthly" }));
    expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ mode: "monthly" }) });
    document.body.innerHTML = "";

    onChange = show(job({ mode: "monthly", at: "05:00" }));
    fireEvent.click(screen.getByRole("button", { name: /time/ }));
    expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ times: ["05:00", "06:00"], at: "05:00" }) });
    fireEvent.click(screen.getByRole("button", { name: "Last" }));
    expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ month_days: [1, -1] }) });
  });

  it("saves a cron expression and a time zone when the field is left, not while typed", async () => {
    const onChange = show(job({ mode: "cron", cron: "" }));
    const cron = screen.getByLabelText("Cron expression");
    fireEvent.change(cron, { target: { value: "30 6 * * 1-5" } });
    expect(onChange).not.toHaveBeenCalled();
    fireEvent.blur(cron);
    await waitFor(() => expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ cron: "30 6 * * 1-5" }) }));
    const tz = screen.getByLabelText("Time zone");
    fireEvent.change(tz, { target: { value: " Europe/Paris " } });
    fireEvent.keyDown(tz, { key: "Enter" });
    await waitFor(() => expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ time_zone: "Europe/Paris" }) }));
    // Only spaces added to the saved value: nothing to save.
    onChange.mockClear();
    fireEvent.change(cron, { target: { value: " 30 6 * * 1-5  " } });
    fireEvent.blur(cron);
    await Promise.resolve();
    expect(onChange).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText("Missed runs"), { target: { value: "each" } });
    expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ missed: "each" }) });
  });

  it("runs after other jobs, not itself", () => {
    useJobs.setState({ jobs: [summary("j1", "Nightly"), summary("j2", "Extract")] });
    const onChange = show(job({ enabled: false }));
    const pick = screen.getByLabelText("Run after another job");
    expect([...pick.querySelectorAll("option")].map((o) => o.textContent)).toEqual(["another job succeeds…", "Extract"]);
    fireEvent.change(pick, { target: { value: "j2" } });
    expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ after_jobs: ["j2"] }) });
  });

  it("warns when an unattended job signs in through the browser", () => {
    useStore.setState({ connections: [conn("c1", "Snowflake SSO", "external_browser"), conn("c2", "Keyed", "key_pair")] });
    show(job());
    expect(screen.getByRole("note").textContent).toContain('"Snowflake SSO" signs in through the browser');
    document.body.innerHTML = "";
    show(job({ enabled: false }));
    expect(screen.queryByRole("note")).toBeNull();
    document.body.innerHTML = "";
    show(job({}, "c2"));
    expect(screen.queryByRole("note")).toBeNull();
  });

  it("keeps focus in a time being edited, sorts and saves it when left", () => {
    const onChange = show(job({ mode: "daily", times: ["06:00", "09:00"], at: "06:00" }));
    const first = screen.getByLabelText("Time 1") as HTMLInputElement;
    first.focus();
    fireEvent.change(first, { target: { value: "10:00" } });
    // Not saved, not re-sorted, the same field still focused while typed.
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByLabelText("Time 1")).toBe(first);
    expect(document.activeElement).toBe(first);
    fireEvent.change(first, { target: { value: "10:30" } });
    expect(document.activeElement).toBe(first);
    fireEvent.blur(first);
    expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ times: ["09:00", "10:30"], at: "09:00" }) });
  });

  it("says a time that is already in the list instead of merging it", () => {
    const onChange = show(job({ mode: "daily", times: ["06:00", "09:00"], at: "06:00" }));
    const first = screen.getByLabelText("Time 1");
    fireEvent.change(first, { target: { value: "09:00" } });
    fireEvent.blur(first);
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByText(/09:00 is already in the list/)).toBeTruthy();
    expect(first.getAttribute("aria-invalid")).toBe("true");
  });

  it("saves a cron or time zone typed but not left when the panel closes", async () => {
    const j = job({ mode: "cron", cron: "0 6 * * *" });
    const onChange = show(j);
    fireEvent.change(screen.getByLabelText("Cron expression"), { target: { value: "30 7 * * 1-5" } });
    onChange.unmount();
    await waitFor(() => expect(onChange).toHaveBeenCalled());
    expect(applied(onChange, j)).toMatchObject({ mode: "cron", cron: "30 7 * * 1-5" });
  });

  it("on close, a typed field doesn't undo a change made elsewhere meanwhile", async () => {
    // Minutes typed, not left; the assistant turns the schedule off (the field goes away).
    const j = job({ minutes: 60 });
    let onChange = show(j);
    fireEvent.change(screen.getByLabelText("Minutes"), { target: { value: "45" } });
    const off = job({ minutes: 60, enabled: false });
    onChange.rerender(off);
    expect(onChange).toHaveBeenCalledTimes(1);
    // Patched alone onto the assistant's version: still off.
    expect(applied(onChange, off)).toMatchObject({ enabled: false, mode: "interval", minutes: 45 });
    // The assistant changed the minutes too: the typed value is dropped.
    expect(onChange.mock.lastCall![0]({ ...off, schedule: { ...off.schedule, minutes: 15 } })).toBeNull();
    document.body.innerHTML = "";

    // A cron typed, not left; the assistant makes it daily: the kind stays daily.
    const c = job({ mode: "cron", cron: "0 6 * * *" });
    onChange = show(c);
    fireEvent.change(screen.getByLabelText("Cron expression"), { target: { value: "30 7 * * *" } });
    const daily = job({ mode: "daily", cron: "0 6 * * *" });
    onChange.rerender(daily);
    await waitFor(() => expect(onChange).toHaveBeenCalled());
    expect(applied(onChange, daily)).toMatchObject({ mode: "daily", cron: "30 7 * * *" });
  });

  it("says a bad cron or time zone at the field and doesn't send it, so the job keeps saving", async () => {
    const onChange = show(job({ mode: "cron", cron: "0 6 * * *" }));
    const cron = screen.getByLabelText("Cron expression");
    for (const [bad, why] of [
      ["61 * * * *", /minute: 61 is out of range/],
      ["0 6 * *", /has 5 fields/],
      ["0 0 30 2 *", /never runs/],
      ["0 0 */0 * *", /bad step/],
    ] as const) {
      fireEvent.change(cron, { target: { value: bad } });
      fireEvent.blur(cron);
      expect(await screen.findByText(why)).toBeTruthy();
      expect(cron.getAttribute("aria-invalid")).toBe("true");
    }
    const tz = screen.getByLabelText("Time zone");
    for (const bad of ["Europe/Pari", "+-5:00", "+25:00"]) {
      fireEvent.change(tz, { target: { value: bad } });
      fireEvent.keyDown(tz, { key: "Enter" });
      await waitFor(() => expect(tz.getAttribute("aria-invalid")).toBe("true"));
    }
    expect(onChange).not.toHaveBeenCalled();
    // Not sent on close either.
    onChange.unmount();
    await new Promise((r) => setTimeout(r, 0));
    expect(onChange).not.toHaveBeenCalled();
  });

  it("the backend's check decides, also for the saved value", async () => {
    // It takes what the local rules don't, and refuses what they'd take.
    vi.spyOn(api, "checkJobCron").mockImplementation(async (c) => (c.startsWith("0 0 29 2") ? 'cron "0 0 29 2 */7" runs too rarely' : null));
    vi.spyOn(api, "checkJobTimeZone").mockImplementation(async (z) => (z === "europe/paris" ? 'unknown time zone "europe/paris"' : null));
    const onChange = show(job({ mode: "cron", cron: "0 6 * * *" }));
    const cron = screen.getByLabelText("Cron expression");
    fireEvent.change(cron, { target: { value: "+5 * * * *" } });
    fireEvent.blur(cron);
    await waitFor(() => expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ cron: "+5 * * * *" }) }));
    const tz = screen.getByLabelText("Time zone");
    fireEvent.change(tz, { target: { value: "europe/paris" } });
    fireEvent.blur(tz);
    expect(await screen.findByText(/unknown time zone "europe\/paris"/)).toBeTruthy();
    expect(onChange).toHaveBeenCalledTimes(1);
    document.body.innerHTML = "";

    // A saved cron a newer rule refuses: said when the panel opens.
    show(job({ mode: "cron", cron: "0 0 29 2 */7" }));
    expect(await screen.findByText(/runs too rarely/)).toBeTruthy();
    expect(screen.getByText(/the job won't save until it's fixed/)).toBeTruthy();
  });

  it("a cron kept from before is checked when Cron is picked", async () => {
    // Invalid: shown with why, nothing saved.
    let onChange = show(job({ mode: "interval", cron: "0 0 30 2 *" }));
    fireEvent.click(screen.getByRole("button", { name: "Cron" }));
    expect(await screen.findByText(/never runs/)).toBeTruthy();
    expect(onChange).not.toHaveBeenCalled();
    document.body.innerHTML = "";
    // Valid: Cron saved at once.
    const j = job({ mode: "interval", cron: "0 6 * * *" });
    onChange = show(j);
    fireEvent.click(screen.getByRole("button", { name: "Cron" }));
    await waitFor(() => expect(onChange).toHaveBeenCalled());
    expect(applied(onChange, j)).toMatchObject({ mode: "cron", cron: "0 6 * * *" });
  });

  it("Cron with no expression yet is shown but not saved until a valid one is typed", async () => {
    const onChange = show(job({ mode: "interval" }));
    fireEvent.click(screen.getByRole("button", { name: "Cron" }));
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Cron" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByText("Next run: after saving")).toBeTruthy();
    const cron = screen.getByLabelText("Cron expression");
    fireEvent.change(cron, { target: { value: "*/15 * * * *" } });
    fireEvent.blur(cron);
    await waitFor(() => expect(onChange).toHaveBeenLastCalledWith({ schedule: expect.objectContaining({ mode: "cron", cron: "*/15 * * * *" }) }));
  });

  it("next run: the saved one's time, 'after saving' while it differs", () => {
    const saved = schedule({ minutes: 60 });
    useJobs.setState({ jobs: [{ ...summary("j1", "Nightly"), schedule: saved }] });
    const at = new Date(2026, 10, 17, 9, 0).getTime();
    show(job({ minutes: 60 }), at);
    expect(screen.getByText(`Next run ${new Date(at).toLocaleString()}`)).toBeTruthy();
    document.body.innerHTML = "";
    show(job({ minutes: 15 }), at);
    expect(screen.getByText("Next run: after saving")).toBeTruthy();
    document.body.innerHTML = "";
    // Saved, nothing due (no "Saving…" forever).
    show(job({ minutes: 60 }), null);
    expect(screen.getByText("No run due")).toBeTruthy();
    document.body.innerHTML = "";
    // A job in its "run after" list deleted since: the saved schedule dropped it, not a difference.
    show(job({ minutes: 60, after_jobs: ["gone"] }), at);
    expect(screen.getByText(`Next run ${new Date(at).toLocaleString()}`)).toBeTruthy();
  });

  it("doesn't offer jobs that already run after this one (a loop)", () => {
    // j2 runs after j1; j3 runs after j2: both would loop. j4 is free.
    useJobs.setState({ jobs: [summary("j1", "Nightly"), summary("j2", "Report", ["j1"]), summary("j3", "Mail", ["j2"]), summary("j4", "Extract")] });
    show(job({ enabled: false }));
    const pick = screen.getByLabelText("Run after another job");
    expect([...pick.querySelectorAll("option")].map((o) => o.textContent)).toEqual(["another job succeeds…", "Extract"]);
  });

  it("minutes: whole numbers from 1 to a year", () => {
    expect(parseMinutes("15")).toBe(15);
    expect(parseMinutes(" 15 ")).toBe(15);
    for (const bad of ["", "0", "-5", "1.5", "2.4", "1e10", "99999999999", String(MAX_MINUTES + 1), "abc"]) expect(parseMinutes(bad), bad).toBeNull();
    const onChange = show(job({ minutes: 60 }));
    const m = screen.getByLabelText("Minutes");
    fireEvent.change(m, { target: { value: "99999999999" } });
    fireEvent.blur(m);
    expect(onChange).not.toHaveBeenCalled();
    expect(m.getAttribute("aria-invalid")).toBe("true");
  });

  it("a step waiting for a slot can be stopped", () => {
    const onStop = vi.fn();
    const node = job().nodes[0];
    const card = (status: string) => (
      <NodeCard node={node} status={{ status } as never} selected={false} dim={false} linking={false} onPointerDown={() => {}} onPortDown={() => {}} onStop={onStop} onContextMenu={() => {}} />
    );
    const { rerender } = render(card("waiting"));
    fireEvent.click(screen.getByRole("button", { name: "Stop orders" }));
    expect(onStop).toHaveBeenCalledOnce();
    rerender(card("success"));
    expect(screen.queryByRole("button", { name: "Stop orders" })).toBeNull();
  });
});
