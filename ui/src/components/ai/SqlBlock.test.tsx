import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { SqlBlock } from "./ChatItems";

describe("SqlBlock", () => {
  it("wraps long lines instead of cutting them at the panel's edge, and keeps the text as it is", () => {
    const sql = "create table bookings (id bigint, booking_date date, amount numeric(18, 2), note text) partition by range (booking_date);\nselect 1";
    render(<SqlBlock code={sql} lang="sql" />);
    const pre = screen.getByText((_, el) => el?.tagName === "PRE" && el.textContent === sql);
    // Wrapped where it can, and inside a long name if it must.
    expect(pre).toHaveClass("whitespace-pre-wrap", "wrap-anywhere");
    expect(pre.textContent).toContain("partition by range (booking_date);");
  });
});
