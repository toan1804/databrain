import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";

vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });
const preview = vi.fn(async (_job: unknown, _node: string, date: string | null) => ({
  kind: "oracle",
  date: date ?? "2026-11-18",
  time: `${date ?? "2026-11-18"} 00:00:00`,
  run_date: `DATE '${date ?? "2026-11-18"}'`,
  run_time: `TIMESTAMP '${date ?? "2026-11-18"} 00:00:00'`,
  sql: `select * from bookings where booking_date = DATE '${date ?? "2026-11-18"}' - 1`,
  examples: [
    { label: "the day before", sql: ":run_date - 1", value: "2026-11-17" },
    { label: "the day before, as text", sql: "to_char(:run_date - 1, 'DD-Mon-YY')", value: "17-Nov-26" },
  ],
}));
vi.mock("../../lib/api", () => ({
  isTauri: () => true,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  api: { runMomentPreview: (...a: [unknown, string, string | null]) => preview(...a), setSetting: async () => {} },
}));

const { RunDateFields, usesRunMoment } = await import("./RunDateFields");
const { useStore } = await import("../../store");

const node = (sql: string) => ({ id: "n1", name: "bookings", kind: "query", connection_id: "ora", sql }) as never;
const job = (n: unknown) => ({ id: "j", name: "J", nodes: [n], edges: [] }) as never;

describe(":run_date in a step", () => {
  it("is found as a parameter, not inside a name or a ::cast", () => {
    expect(usesRunMoment("where d = :run_date - 1")).toBe(true);
    expect(usesRunMoment("where t < :RUN_TIME")).toBe(true);
    expect(usesRunMoment("where d = :run_dates")).toBe(false);
    expect(usesRunMoment("select x::run_date")).toBe(false);
    expect(usesRunMoment("select 1")).toBe(false);
  });

  it("shows what the database gets, ways to write the day before, and the SQL for another date to try", async () => {
    vi.useFakeTimers();
    try {
      const n = node("select * from bookings where booking_date = :run_date - 1");
      render(<RunDateFields job={job(n)} node={n} />);
      await act(async () => void (await vi.advanceTimersByTimeAsync(350)));
      expect(screen.getByText("DATE '2026-11-18'")).toBeTruthy();
      expect(screen.getByText("to_char(:run_date - 1, 'DD-Mon-YY')")).toBeTruthy();
      expect(screen.getByText("→ 17-Nov-26")).toBeTruthy();
      expect(screen.getByText(/Month names follow the session's language/)).toBeTruthy();

      fireEvent.change(screen.getByLabelText("Run for date"), { target: { value: "2026-01-01" } });
      await act(async () => void (await vi.advanceTimersByTimeAsync(350)));
      expect(preview).toHaveBeenLastCalledWith(expect.anything(), "n1", "2026-01-01");
      act(() => screen.getByRole("button", { name: /Open in editor/ }).click());
      const tab = useStore.getState().tabs.find((t) => t.title === "bookings for 2026-01-01");
      expect(tab?.sql).toBe("select * from bookings where booking_date = DATE '2026-01-01' - 1");
      expect(tab?.connection_id).toBe("ora");
    } finally {
      vi.useRealTimers();
    }
  });

  it("is not shown for SQL without a run date or today's date", () => {
    const n = node("select 1");
    const { container } = render(<RunDateFields job={job(n)} node={n} />);
    expect(container.textContent).toBe("");
  });
});
