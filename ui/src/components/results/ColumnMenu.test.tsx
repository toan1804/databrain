import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ColumnStats, ResultInfo, ViewSpec } from "../../lib/types";
import { emptyView } from "../../lib/util";

const asked: ViewSpec[] = [];
vi.mock("../../lib/api", () => ({
  api: {
    columnStats: (_id: string, view: ViewSpec, column: number) => {
      asked.push(view);
      return Promise.resolve({ column, count: 1200, nulls: 0, distinct: 1200, min: "0", max: "1199", top: [] } as ColumnStats);
    },
  },
  toError: (e: unknown) => e as Error,
}));

const { ColumnMenu } = await import("./ColumnMenu");

const col = { name: "id", data_type: "Int64", db_type: null, family: "number", nullable: false };
const info = (on_disk: boolean) => ({ id: "r", columns: [col], total_rows: 5_000_000, complete: true, truncated: false, bytes: 0, on_disk, disk_bytes: 0 }) as unknown as ResultInfo;

describe("ColumnMenu statistics", () => {
  it("of a result on disk: the rows loaded in the grid, and says so", async () => {
    render(<ColumnMenu info={info(true)} col={0} x={0} y={0} view={emptyView()} statsView={{ ...emptyView(), first_rows: 1200 }} onView={() => {}} onClose={() => {}} />);
    expect(await screen.findByRole("note")).toHaveTextContent("Of the rows loaded in the grid only, not all 5,000,000");
    expect(asked.at(-1)?.first_rows).toBe(1200);
  });

  it("of a result in memory: every row, no note", async () => {
    render(<ColumnMenu info={info(false)} col={0} x={0} y={0} view={emptyView()} onView={() => {}} onClose={() => {}} />);
    expect(await screen.findAllByText("1,200")).toHaveLength(2);
    expect(screen.queryByRole("note")).not.toBeInTheDocument();
    expect(asked.at(-1)?.first_rows ?? null).toBeNull();
  });
});
