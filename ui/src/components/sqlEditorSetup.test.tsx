import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, runScopeHandlers } from "@codemirror/view";
import { afterEach, describe, expect, it, vi } from "vitest";
import { sqlEditorExtensions, type SqlEditorSetup } from "./sqlEditorSetup";

const views: EditorView[] = [];
afterEach(() => {
  views.splice(0).forEach((v) => v.destroy());
  document.body.innerHTML = "";
});

function editor(o: Partial<SqlEditorSetup> = {}) {
  const parent = document.createElement("div");
  document.body.appendChild(parent);
  let view: EditorView | null = null;
  const onDocChange = vi.fn();
  view = new EditorView({
    parent,
    state: EditorState.create({
      doc: "select 1",
      extensions: sqlEditorExtensions({
        editorKey: "k1",
        connectionId: () => null,
        view: () => view,
        lang: new Compartment(),
        placeholder: "SQL…",
        ariaLabel: "SQL cell",
        onMenu: () => {},
        onDocChange,
        ...o,
      }),
    }),
  });
  views.push(view);
  return { view, onDocChange };
}

// jsdom isn't a Mac: Mod is Ctrl.
const key = (view: EditorView, init: KeyboardEventInit) => runScopeHandlers(view, new KeyboardEvent("keydown", init), "editor");

describe("shared SQL editor setup", () => {
  it("gives query tabs gutters and notebook cells none", () => {
    expect(editor({ gutters: true }).view.dom.querySelector(".cm-lineNumbers")).not.toBeNull();
    expect(editor().view.dom.querySelector(".cm-lineNumbers")).toBeNull();
  });

  it("labels the editor, reports edits and opens inline AI for its key", () => {
    const { view, onDocChange } = editor();
    expect(view.contentDOM).toHaveAttribute("aria-label", "SQL cell");
    view.dispatch({ changes: { from: view.state.doc.length, insert: " + 1" } });
    expect(onDocChange).toHaveBeenLastCalledWith("select 1 + 1");
    const seen: string[] = [];
    const on = (e: Event) => seen.push((e as CustomEvent).detail.key);
    window.addEventListener("db:inline-ai", on);
    expect(key(view, { key: "i", ctrlKey: true })).toBe(true);
    window.removeEventListener("db:inline-ai", on);
    expect(seen).toEqual(["k1"]);
  });

  it("runs the editor's own keys before the shared ones", () => {
    const run = vi.fn(() => true);
    const { view } = editor({ keys: [{ key: "Mod-Enter", run }] });
    expect(key(view, { key: "Enter", ctrlKey: true })).toBe(true);
    expect(run).toHaveBeenCalledOnce();
    // The edit didn't reach defaultKeymap's Mod-Enter (insert a blank line).
    expect(view.state.doc.toString()).toBe("select 1");
  });
});
