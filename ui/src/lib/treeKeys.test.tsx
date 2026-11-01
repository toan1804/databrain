import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState, type ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { treeKeyDown } from "./treeKeys";

function Node({ label, depth, children }: { label: string; depth: number; children?: ReactNode }) {
  const [open, setOpen] = useState(false);
  const expanded = children ? open : undefined;
  return (
    <>
      <div role="treeitem" aria-expanded={expanded} tabIndex={0} style={{ paddingLeft: 4 + depth * 14 }} onKeyDown={(e) => treeKeyDown(e, { expanded, toggle: () => setOpen((o) => !o) })}>
        {label}
      </div>
      {open && children}
    </>
  );
}

/** Like the explorer: a folder's connections are wrapped in an indented div and start at depth 0. */
function Explorer() {
  return (
    <div role="tree" aria-label="Connections">
      <Node label="Prod" depth={0}>
        <div style={{ paddingLeft: 14 }}>
          <Node label="shop" depth={0}>
            <Node label="public" depth={1}>
              <Node label="orders" depth={2} />
            </Node>
          </Node>
        </div>
      </Node>
      <Node label="local" depth={0} />
    </div>
  );
}

const item = (name: string) => screen.getByRole("treeitem", { name });

describe("explorer arrow keys", () => {
  it("moves, expands and collapses like a tree", async () => {
    const user = userEvent.setup();
    render(<Explorer />);
    item("Prod").focus();
    await user.keyboard("{ArrowDown}");
    expect(item("local")).toHaveFocus();
    await user.keyboard("{ArrowUp}{ArrowRight}");
    expect(item("Prod")).toHaveAttribute("aria-expanded", "true");
    expect(item("Prod")).toHaveFocus();
    // → on an open row moves into it.
    await user.keyboard("{ArrowRight}");
    expect(item("shop")).toHaveFocus();
    await user.keyboard("{ArrowRight}{ArrowRight}{ArrowRight}{ArrowRight}");
    expect(item("orders")).toHaveFocus();
    // ← on a leaf goes to its parent, through the folder's wrapper too.
    await user.keyboard("{ArrowLeft}");
    expect(item("public")).toHaveFocus();
    // ← on an open row closes it, then moves to the parent.
    await user.keyboard("{ArrowLeft}{ArrowLeft}");
    expect(item("shop")).toHaveFocus();
    expect(screen.queryByRole("treeitem", { name: "orders" })).not.toBeInTheDocument();
    await user.keyboard("{ArrowLeft}{ArrowLeft}");
    expect(item("Prod")).toHaveFocus();
    await user.keyboard("{End}");
    expect(item("local")).toHaveFocus();
    await user.keyboard("{Home}");
    expect(item("Prod")).toHaveFocus();
  });
});
