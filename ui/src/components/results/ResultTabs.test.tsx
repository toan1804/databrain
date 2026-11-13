import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { StatementRun } from "../../store";
import { ResultTabs } from "./ResultTabs";

const stmt = (i: number): StatementRun =>
  ({
    plan: { index: i, sql: i === 11 ? "select * from orders_by_month" : `select ${i}` },
    status: "done",
    notices: [],
    result: { id: `r${i}`, total_rows: i * 10, columns: [] },
  }) as unknown as StatementRun;

function Harness({ n }: { n: number }) {
  const statements = Array.from({ length: n }, (_, i) => stmt(i));
  const [active, setActive] = useState<number | null>(n - 1);
  return (
    <>
      <ResultTabs statements={statements} active={active} onSelect={setActive} />
      <output aria-label="shown">{active}</output>
    </>
  );
}

describe("ResultTabs", () => {
  it("lists 15 results as tabs and moves between them with the keyboard", async () => {
    const user = userEvent.setup();
    render(<Harness n={15} />);
    const tabs = within(screen.getByRole("tablist", { name: "Results" })).getAllByRole("tab");
    expect(tabs).toHaveLength(15);
    expect(tabs[14]).toHaveAttribute("aria-selected", "true");
    tabs[14].focus();
    await user.keyboard("{Home}");
    expect(screen.getByLabelText("shown")).toHaveTextContent("0");
    await user.keyboard("{ArrowRight}{ArrowRight}");
    expect(screen.getByLabelText("shown")).toHaveTextContent("2");
    await user.keyboard("{End}");
    expect(screen.getByLabelText("shown")).toHaveTextContent("14");
    await user.click(tabs[4]);
    expect(screen.getByLabelText("shown")).toHaveTextContent("4");
  });

  it("finds a result in the list of all results by its SQL or number", async () => {
    const user = userEvent.setup();
    render(<Harness n={15} />);
    await user.click(screen.getByRole("button", { name: "All results" }));
    const find = screen.getByRole("textbox", { name: "Find a result" });
    expect(within(screen.getByRole("listbox")).getAllByRole("option")).toHaveLength(15);
    await user.type(find, "orders_by");
    expect(within(screen.getByRole("listbox")).getAllByRole("option")).toHaveLength(1);
    await user.keyboard("{Enter}");
    expect(screen.getByLabelText("shown")).toHaveTextContent("11");
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    // By number (as shown on the tab: 1-based), with a click.
    await user.click(screen.getByRole("button", { name: "All results" }));
    await user.type(screen.getByRole("textbox", { name: "Find a result" }), "7");
    await user.click(within(screen.getByRole("listbox")).getByRole("option"));
    expect(screen.getByLabelText("shown")).toHaveTextContent("6");
  });

  it("offers the list only when there are many results or they don't fit", () => {
    render(<Harness n={3} />);
    expect(screen.queryByRole("button", { name: "All results" })).not.toBeInTheDocument();
  });

  it("shows kept results first, unnumbered, and lets go of one", async () => {
    const user = userEvent.setup();
    const kept = { ...stmt(0), plan: { index: -1, sql: "select * from orders_by_month" } } as StatementRun;
    const onRelease = vi.fn();
    const onSelect = vi.fn();
    render(<ResultTabs statements={[kept, stmt(0), stmt(1)]} active={0} onSelect={onSelect} onRelease={onRelease} />);
    const tabs = screen.getAllByRole("tab");
    expect(tabs).toHaveLength(3);
    expect(tabs[0]).toHaveTextContent("select * from orders");
    expect(tabs[0]).not.toHaveTextContent(/^0\./);
    expect(tabs[1]).toHaveTextContent("1. Result");
    await user.click(tabs[0]);
    expect(onSelect).toHaveBeenCalledWith(-1);
    await user.click(screen.getByRole("button", { name: /Stop keeping/ }));
    expect(onRelease).toHaveBeenCalledWith(kept);
  });
});
