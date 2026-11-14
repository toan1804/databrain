import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ResultInfo } from "../../lib/types";
import { emptyView } from "../../lib/util";
import { ResultFooter } from "./ResultFooter";

const info = { id: "r", columns: [], total_rows: 5_000_000, complete: true, truncated: false, bytes: 0, on_disk: true, disk_bytes: 1 << 30 } as ResultInfo;
const sorted = { ...emptyView(), sort: [{ column: 0, descending: false }], first_rows: 1200 };

describe("ResultFooter", () => {
  it("says a sort of a result on disk covers only the rows loaded in the grid", () => {
    render(<ResultFooter stmt={{}} info={info} view={sorted} mode="chart" filtered={false} viewRows={1200} windowRows={1200} selRange={null} />);
    expect(screen.getByText("first 1,200 of 5,000,000 rows sorted")).toHaveAttribute("title", expect.stringContaining("rows loaded in the grid"));
  });

  it("counts filtered rows of the first rows", () => {
    render(<ResultFooter stmt={{}} info={info} view={sorted} mode="chart" filtered viewRows={30} windowRows={1200} selRange={null} />);
    expect(screen.getByText("30 of the first 1,200 rows")).toBeInTheDocument();
  });

  it("counts every row without a window", () => {
    render(<ResultFooter stmt={{}} info={info} view={emptyView()} mode="chart" filtered={false} viewRows={5_000_000} selRange={null} />);
    expect(screen.getByText("5,000,000 rows")).toBeInTheDocument();
  });
});
