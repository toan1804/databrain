import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ChartData, ResultInfo, ViewSpec } from "../lib/types";
import { emptyView } from "../lib/util";

const asked: ViewSpec[] = [];
vi.mock("../lib/api", () => ({
  api: {
    chartData: (_id: string, view: ViewSpec) => {
      asked.push(view);
      return Promise.resolve({ x: ["a", "b"], x_family: "text", series: [{ name: "n", values: [1, 2] }], truncated: false, rows: 1200, window_rows: view.first_rows ?? null } as ChartData);
    },
  },
  toError: (e: unknown) => e as Error,
}));

const { ChartView } = await import("./ChartView");

const col = { name: "c", data_type: "Utf8", db_type: null, family: "text", nullable: true };
const info = (on_disk: boolean) => ({ id: "r", columns: [col], total_rows: 5_000_000, complete: true, truncated: false, bytes: 0, on_disk, disk_bytes: 0 }) as unknown as ResultInfo;

describe("ChartView", () => {
  it("charts a result on disk from the rows loaded in the grid, says so, and doesn't pin it", async () => {
    render(<ChartView info={info(true)} view={{ ...emptyView(), first_rows: 1200 }} onPin={() => {}} />);
    expect(await screen.findByRole("note")).toHaveTextContent("charted from the first 1,200 of 5,000,000 rows");
    expect(asked.at(-1)?.first_rows).toBe(1200);
    const pin = screen.getByRole("button", { name: /Pin to dashboard/ });
    expect(pin).toBeDisabled();
    expect(pin).toHaveAttribute("title", expect.stringContaining("Too large to pin"));
  });

  it("charts a result in memory from every row, and pins it", async () => {
    render(<ChartView info={info(false)} view={emptyView()} onPin={() => {}} />);
    await vi.waitFor(() => expect(asked.at(-1)?.first_rows ?? null).toBeNull());
    expect(screen.queryByRole("note")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Pin to dashboard/ })).toBeEnabled();
  });
});
