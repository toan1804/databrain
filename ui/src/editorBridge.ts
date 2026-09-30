import type { EditorView } from "@codemirror/view";
import type { EditProposal } from "./lib/types";

/**
 * Live CodeMirror views by key: SQL tab id, or `nb:{notebook}:{cell}` for
 * notebook cells. Used by sidebar inserts, palette actions and the AI agent.
 */
const views = new Map<string, EditorView>();
let lastFocused: string | null = null;

export const editorBridge = {
  register(key: string, view: EditorView) {
    views.set(key, view);
  },
  unregister(key: string, view: EditorView) {
    if (views.get(key) === view) views.delete(key);
  },
  get(key: string | null | undefined): EditorView | undefined {
    return key ? views.get(key) : undefined;
  },
  setFocused(key: string) {
    lastFocused = key;
  },
  /** Most recently focused editor key that still exists. */
  focused(): string | null {
    return lastFocused && views.has(lastFocused) ? lastFocused : null;
  },
  insert(key: string | null | undefined, text: string) {
    const v = this.get(key);
    if (!v) return;
    const { from, to } = v.state.selection.main;
    v.dispatch({
      changes: { from, to, insert: text },
      selection: { anchor: from + text.length },
      scrollIntoView: true,
    });
    v.focus();
  },
  focus(key: string | null | undefined) {
    this.get(key)?.focus();
  },
  /** Editor snapshot for the AI (`get_editor`). */
  snapshot(key: string | null | undefined): { sql: string; selection: string | null } | null {
    const v = this.get(key);
    if (!v) return null;
    const { from, to } = v.state.selection.main;
    return { sql: v.state.doc.toString(), selection: to > from ? v.state.sliceDoc(from, to) : null };
  },
  /** Apply an accepted AI proposal. Returns false if the editor is gone. */
  apply(key: string | null | undefined, p: EditProposal): boolean {
    const v = this.get(key);
    if (!v) return false;
    const { from, to } = v.state.selection.main;
    const len = v.state.doc.length;
    const change =
      p.mode === "replace_all"
        ? { from: 0, to: len, insert: p.sql }
        : p.mode === "insert_at_cursor"
          ? { from: to, to, insert: p.sql }
          : to > from
            ? { from, to, insert: p.sql }
            : { from: 0, to: len, insert: p.sql };
    v.dispatch({
      changes: change,
      selection: { anchor: change.from, head: change.from + p.sql.length },
      scrollIntoView: true,
    });
    v.focus();
    return true;
  },
};
