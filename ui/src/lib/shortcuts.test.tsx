import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { globalRunApplies } from "./shortcuts";

/** The guard as the global keydown listener sees a ⌘↵ press on `el`. */
function pressOn(el: Element, handled = false): boolean {
  let applies: boolean | null = null;
  const listener = (e: KeyboardEvent) => {
    applies = globalRunApplies(e);
  };
  window.addEventListener("keydown", listener);
  const ev = new KeyboardEvent("keydown", { key: "Enter", metaKey: true, bubbles: true, cancelable: true });
  if (handled) el.addEventListener("keydown", (e) => e.preventDefault(), { once: true });
  el.dispatchEvent(ev);
  window.removeEventListener("keydown", listener);
  return applies!;
}

describe("global ⌘↵", () => {
  it("runs the tab from the grid or the page, not from text fields", () => {
    const { getByTestId } = render(
      <div>
        <div data-testid="grid" tabIndex={0} />
        <input data-testid="input" />
        <textarea data-testid="textarea" />
        <select data-testid="select" />
        <div data-testid="rich" contentEditable suppressContentEditableWarning />
        <div className="cm-editor">
          <div data-testid="editor" className="cm-content" contentEditable suppressContentEditableWarning />
        </div>
        <input data-testid="param" data-run-shortcut />
      </div>,
    );
    expect(pressOn(getByTestId("grid"))).toBe(true);
    expect(pressOn(document.body)).toBe(true);
    for (const id of ["input", "textarea", "select", "rich", "editor"]) expect(pressOn(getByTestId(id)), id).toBe(false);
    // The tab's parameter bar opts in.
    expect(pressOn(getByTestId("param"))).toBe(true);
  });

  it("leaves a key a component already handled", () => {
    const { getByTestId } = render(<div data-testid="cell" tabIndex={0} />);
    expect(pressOn(getByTestId("cell"), true)).toBe(false);
  });
});
