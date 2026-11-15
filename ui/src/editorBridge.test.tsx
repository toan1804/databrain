import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { afterEach, describe, expect, it } from "vitest";
import { editorBridge } from "./editorBridge";

// jsdom has no layout; CodeMirror measures text ranges.
Range.prototype.getClientRects = () => Object.assign([], { item: () => null }) as unknown as DOMRectList;
Range.prototype.getBoundingClientRect = () => new DOMRect();

const views: EditorView[] = [];
function editor(doc: string, from = 0, to = from): EditorView {
  const v = new EditorView({ state: EditorState.create({ doc, selection: { anchor: from, head: to } }), parent: document.body });
  editorBridge.register("t", v);
  views.push(v);
  return v;
}
afterEach(() => {
  for (const v of views.splice(0)) {
    editorBridge.unregister("t", v);
    v.destroy();
  }
});

const proposal = (mode: "replace_all" | "replace_selection" | "insert_at_cursor", sql: string) => ({ mode, sql, title: null }) as never;

describe("applying an AI edit", () => {
  it("replaces the whole text only when it is still the text the edit was made for", () => {
    const v = editor("select 1");
    expect(editorBridge.apply("t", proposal("replace_all", "select 2"), { selection: null, doc: "select 1" })).toBe(true);
    expect(v.state.doc.toString()).toBe("select 2");
    // Typed in since: refused, nothing lost.
    v.dispatch({ changes: { from: 0, insert: "-- mine\n" } });
    expect(editorBridge.apply("t", proposal("replace_all", "select 3"), { selection: null, doc: "select 2" })).toBe(false);
    expect(v.state.doc.toString()).toBe("-- mine\nselect 2");
  });

  it("an edit made with no selection (an empty editor too) is a whole-text edit", () => {
    const v = editor("");
    v.dispatch({ changes: { from: 0, insert: "select * from big_script" } });
    expect(editorBridge.apply("t", proposal("replace_selection", "select 1"), { selection: null, doc: "" })).toBe(false);
    expect(v.state.doc.toString()).toBe("select * from big_script");
  });

  it("replaces the selection it was made for, also after the cursor moved, and refuses when it's gone", () => {
    const v = editor("select a;\nselect b;", 0, 8);
    v.dispatch({ selection: { anchor: 15 } });
    expect(editorBridge.apply("t", proposal("replace_selection", "select x"), { selection: "select a", doc: "select a;\nselect b;" })).toBe(true);
    expect(v.state.doc.toString()).toBe("select x;\nselect b;");
    expect(editorBridge.apply("t", proposal("replace_selection", "select y"), { selection: "select a", doc: "" })).toBe(false);
    expect(v.state.doc.toString()).toBe("select x;\nselect b;");
  });
});
