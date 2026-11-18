import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { ProgressBar, Warnings, progressDetail, roughTime, statusText } from "./shared";

const p = (done: number, total: number, elapsed_ms: number) => ({ phase: "Inserting rows", done, total, elapsed_ms });

describe("a running step's progress", () => {
  it("gives the rate and the time left when the total is known", () => {
    expect(progressDetail(p(250_000, 1_000_000, 10_000))).toBe("250,000 / 1,000,000 rows · 25% · 25,000 rows/s · about 30s left");
    expect(progressDetail(p(100_000, 10_000_000, 20_000))).toBe("100,000 / 10,000,000 rows · 1% · 5,000 rows/s · about 33m 00s left");
    // Too early for a rate: no guess.
    expect(progressDetail(p(0, 1_000, 0))).toBe("0 / 1,000 rows · 0%");
  });

  it("gives rows so far and the time taken when the total isn't known", () => {
    expect(progressDetail({ ...p(1_200_000, 0, 60_000), phase: "Reading rows" })).toBe("1,200,000 rows · 20,000 rows/s · 1m 00s");
    expect(progressDetail(p(500, 0, 200))).toBe("500 rows");
  });

  it("says the database is still at work once every row is sent", () => {
    expect(progressDetail(p(1_000, 1_000, 5_000))).toBe("all 1,000 rows sent, waiting for the database to finish");
    expect(statusText({ status: "running", finished_at: 0, duration_ms: 0, progress: p(1_000, 1_000, 5_000) })).toBe(
      "Inserting rows: all 1,000 rows sent, waiting for the database to finish",
    );
  });

  it("rounds estimates to the second, minute or hour", () => {
    expect([roughTime(400), roughTime(59_400), roughTime(185_000), roughTime(4_830_000)]).toEqual(["1s", "59s", "3m 05s", "1h 20m"]);
  });

  it("pulses a full bar, without a value, when the end isn't known", () => {
    render(<ProgressBar p={{ ...p(1_200, 0, 3_000), phase: "Reading rows" }} />);
    const bar = screen.getByRole("progressbar");
    expect(bar.getAttribute("aria-valuenow")).toBeNull();
    expect(bar.getAttribute("aria-valuetext")).toBe("Reading rows: 1,200 rows · 400 rows/s · 3s");
    expect(bar.firstElementChild?.className).toContain("animate-pulse");
  });
});

describe("a load's warnings", () => {
  it("show each as a note, and nothing without any", () => {
    const w = "Inserting rows: INSERTs, not a bulk load (booking_date (Timestamp(Microsecond, None)) into a date column). To load in bulk, give that column of the rows the table's type in the SQL that selects them (a cast)";
    const { container, rerender } = render(<Warnings warnings={[w]} />);
    expect(screen.getByRole("note").textContent).toContain("booking_date");
    rerender(<Warnings warnings={[]} />);
    expect(container.textContent).toBe("");
  });
});
