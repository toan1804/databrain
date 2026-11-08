// Inline AI completion ("ghost text") for the SQL editor: after a pause in
// typing, the AI provider's fast model suggests how the statement goes on;
// it shows greyed at the cursor. Tab accepts it, ⌘→ accepts a word, Esc
// dismisses it, typing what it says keeps the rest. ⌥\ asks right away.
import { hasNextSnippetField } from "@codemirror/autocomplete";
import { inlineTarget } from "../lib/inlineAi";
import { Prec, StateEffect, StateField, type EditorState, type Extension } from "@codemirror/state";
import { Decoration, EditorView, ViewPlugin, WidgetType, keymap, type ViewUpdate } from "@codemirror/view";
import { api, isTauri, toError } from "../lib/api";
import { advanceGhost, canSuggest, fitGhost, ghostWindow, nextWord } from "../lib/ghostText";
import { traitsOf } from "../lib/sourceTraits";
import { uid } from "../lib/util";
import { useAi } from "../aiStore";
import { useStore } from "../store";

const DEBOUNCE_MS = 700;

interface Ghost {
  pos: number;
  text: string;
}

export const setGhost = StateEffect.define<Ghost | null>();

class GhostWidget extends WidgetType {
  constructor(readonly text: string) {
    super();
  }
  eq(o: GhostWidget) {
    return o.text === this.text;
  }
  toDOM() {
    const s = document.createElement("span");
    s.className = "cm-ghost";
    s.textContent = this.text;
    s.setAttribute("aria-hidden", "true");
    return s;
  }
  ignoreEvent() {
    return true;
  }
}

const ghostField = StateField.define<Ghost | null>({
  create: () => null,
  update(g, tr) {
    for (const e of tr.effects) if (e.is(setGhost)) return e.value;
    if (!g) return null;
    if (tr.docChanged) {
      // Typing the suggestion's own text keeps the rest of it.
      let typed: string | null = null;
      let other = false;
      tr.changes.iterChanges((fromA, toA, _fromB, _toB, ins) => {
        if (fromA === g.pos && toA === g.pos && typed === null) typed = ins.toString();
        else other = true;
      });
      const rest = !other && typed !== null ? advanceGhost(g.text, typed) : null;
      return rest && typed !== null ? { pos: g.pos + (typed as string).length, text: rest } : null;
    }
    if (tr.selection) {
      const m = tr.state.selection.main;
      if (!m.empty || m.head !== g.pos) return null;
    }
    return g;
  },
  provide: (f) => EditorView.decorations.from(f, (g) => (g ? Decoration.set([Decoration.widget({ widget: new GhostWidget(g.text), side: 1 }).range(g.pos)]) : Decoration.none)),
});

/** The shown suggestion, if any. */
export const ghostOf = (s: EditorState) => s.field(ghostField, false) ?? null;

function accept(view: EditorView, wordOnly: boolean): boolean {
  const g = ghostOf(view.state);
  if (!g) return false;
  const text = wordOnly ? nextWord(g.text) : g.text;
  const rest = wordOnly ? g.text.slice(text.length) : "";
  view.dispatch({
    changes: { from: g.pos, insert: text },
    selection: { anchor: g.pos + text.length },
    effects: setGhost.of(rest.trim() ? { pos: g.pos + text.length, text: rest } : null),
    userEvent: "input.complete",
  });
  return true;
}

/** Messages already shown (a broken provider is reported once, not on every pause). */
const reported = new Set<string>();

function ghostPlugin(getConnId: () => string | null | undefined) {
  return ViewPlugin.fromClass(
    class {
      timer: ReturnType<typeof setTimeout> | undefined;
      request: string | null = null;
      seq = 0;
      constructor(readonly view: EditorView) {}

      update(u: ViewUpdate) {
        if (!u.docChanged && !u.selectionSet) return;
        this.cancel();
        // Only after typing (not when the cursor just moves), and not while a suggestion is still shown.
        const typed = u.transactions.some((t) => t.isUserEvent("input") || t.isUserEvent("delete"));
        if (!u.docChanged || !typed || ghostOf(u.state)) return;
        if (!useStore.getState().inlineAiCompletion) return;
        this.timer = setTimeout(() => void this.fetch(false), DEBOUNCE_MS);
      }

      cancel() {
        clearTimeout(this.timer);
        this.seq++;
        if (this.request) api.aiCancel(this.request).catch(() => {});
        this.request = null;
      }

      async fetch(explicit: boolean) {
        const view = this.view;
        const state = view.state;
        const sel = state.selection.main;
        if (!isTauri() || !sel.empty || hasNextSnippetField(state)) return;
        const st = useStore.getState();
        const conn = st.connections.find((c) => c.id === getConnId());
        if (!conn || !conn.ai_policy.ai_enabled || traitsOf(conn.config.kind).language !== "sql") return;
        const doc = state.doc.toString();
        const pos = sel.head;
        if (!explicit && !canSuggest(doc, pos)) return;
        const { prefix, suffix } = ghostWindow(doc, pos);
        const ai = useAi.getState();
        const id = uid();
        const my = ++this.seq;
        this.request = id;
        try {
          const target = inlineTarget(st.inlineAiProvider, ai.providers, ai);
          const raw = await api.aiGhost(id, { connection_id: conn.id, ...target, prefix, suffix });
          if (my !== this.seq || view.state.doc.toString() !== doc || view.state.selection.main.head !== pos) return;
          const text = fitGhost(raw, prefix, suffix);
          if (text) view.dispatch({ effects: setGhost.of({ pos, text }) });
        } catch (e) {
          const err = toError(e);
          if (err.kind === "cancelled" || my !== this.seq) return;
          const msg = `Inline completion: ${err.message}`;
          if (explicit || !reported.has(msg)) {
            reported.add(msg);
            useStore.getState().toast(msg, "error");
          }
        } finally {
          if (this.request === id) this.request = null;
        }
      }

      destroy() {
        this.cancel();
      }
    },
  );
}

/** Ghost completion for the connection returned by `getConnId` (read on each request). */
export function ghostCompletion(getConnId: () => string | null | undefined): Extension {
  const plugin = ghostPlugin(getConnId);
  return [
    ghostField,
    plugin,
    Prec.highest(
      keymap.of([
        { key: "Tab", run: (v) => accept(v, false) },
        { key: "Mod-ArrowRight", run: (v) => accept(v, true) },
        {
          key: "Escape",
          run: (v) => {
            if (!ghostOf(v.state)) return false;
            v.dispatch({ effects: setGhost.of(null) });
            return true;
          },
        },
        {
          key: "Alt-\\",
          run: (v) => {
            const p = v.plugin(plugin);
            if (!p) return false;
            p.cancel();
            void p.fetch(true);
            return true;
          },
        },
      ]),
    ),
    EditorView.baseTheme({ ".cm-ghost": { opacity: "0.45", fontStyle: "italic", whiteSpace: "pre", pointerEvents: "none" } }),
  ];
}
