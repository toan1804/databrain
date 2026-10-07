// `:name` parameters in the SQL editor: highlighted in the text, listed in
// the parameter bar above it (values are bound by the engine on run).
import { StateEffect, StateField } from "@codemirror/state";
import { Decoration, EditorView, type DecorationSet } from "@codemirror/view";
import type { ParamSpan } from "../lib/types";

export const setParamSpans = StateEffect.define<ParamSpan[]>();

/** Marks the `:name` tokens the backend found (kept in place while typing). */
export const paramField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(deco, tr) {
    for (const e of tr.effects) {
      if (e.is(setParamSpans)) {
        const len = tr.state.doc.length;
        return Decoration.set(
          e.value
            .filter((p) => p.to <= len && p.from < p.to)
            .map((p) => Decoration.mark({ class: "cm-param", attributes: { title: `Parameter :${p.name}: set its value above the editor` } }).range(p.from, p.to)),
          true,
        );
      }
    }
    return tr.docChanged ? deco.map(tr.changes) : deco;
  },
  provide: (f) => EditorView.decorations.from(f),
});

/** Unique names in order of first use. */
export function paramNames(spans: ParamSpan[]): string[] {
  return [...new Set(spans.map((s) => s.name))];
}
