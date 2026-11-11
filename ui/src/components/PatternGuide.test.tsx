import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { PatternGuide } from "./PatternGuide";

describe("pattern guide", () => {
  it("shows the syntax by column type with examples for the field last clicked", () => {
    const onPick = vi.fn();
    render(<PatternGuide target="email" onPick={onPick} />);
    expect(screen.getByText("Text")).toBeTruthy();
    expect(screen.getByText(/to use it for/)).toBeTruthy();
    fireEvent.click(screen.getByTitle("Use %@gmail.com for email"));
    expect(onPick).toHaveBeenCalledWith("%@gmail.com");
  });

  it("asks for a field first", () => {
    const onPick = vi.fn();
    render(<PatternGuide target={null} onPick={onPick} />);
    expect(screen.getByText(/Click a column's pattern field, then an example/)).toBeTruthy();
    const ex = screen.getAllByTitle("Click a column's pattern field first")[0] as HTMLButtonElement;
    expect(ex.disabled).toBe(true);
  });
});
