// The CodeMirror setup shared by every SQL editor: query tabs (SqlEditor)
// and notebook cells (CellEditor). Add an editor feature here once and both
// get it; what differs (gutters, run keys, saving) is passed in.

import { useEffect, type RefObject } from "react";
import { closeBrackets, closeBracketsKeymap, completionKeymap } from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { HighlightStyle, bracketMatching, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { StateEffect, StateField, type Compartment, type Extension } from "@codemirror/state";
import {
  Decoration,
  EditorView,
  drawSelection,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
  placeholder,
  type DecorationSet,
  type KeyBinding,
} from "@codemirror/view";
import { tags as t } from "@lezer/highlight";
import { api, isTauri } from "../lib/api";
import { useEvent } from "../lib/hooks";
import type { ConnectionView, ConnectorKind, ParamSpan } from "../lib/types";
import { useStore } from "../store";
import { editorBridge } from "../editorBridge";
import { ghostCompletion } from "./ghostExt";
import { paramField, setParamSpans } from "./paramsExt";
import { queryHints } from "./queryHintsExt";
import { formatKey } from "./SqlContextMenu";
import { canFetchMetadata, sqlAssist, sqlLanguage } from "./sqlAssist";
import { sqlFolding } from "./sqlFold";

/** Right-click: keep a selection that contains the click, else put the cursor there; then open the menu. */
export function editorContextMenu(open: (at: { x: number; y: number }) => void) {
  return EditorView.domEventHandlers({
    contextmenu: (e, view) => {
      e.preventDefault();
      const pos = view.posAtCoords({ x: e.clientX, y: e.clientY });
      const sel = view.state.selection.main;
      if (pos !== null && (sel.empty || pos < sel.from || pos > sel.to)) view.dispatch({ selection: { anchor: pos } });
      open({ x: e.clientX, y: e.clientY });
      return true;
    },
  });
}

export const highlight = HighlightStyle.define([
  { tag: [t.keyword, t.operatorKeyword, t.modifier], color: "var(--syn-keyword)", fontWeight: "500" },
  { tag: [t.string, t.special(t.string)], color: "var(--syn-string)" },
  { tag: [t.number, t.bool, t.null], color: "var(--syn-number)" },
  { tag: [t.lineComment, t.blockComment, t.comment], color: "var(--syn-comment)", fontStyle: "italic" },
  { tag: [t.typeName, t.standard(t.name)], color: "var(--syn-type)" },
  { tag: [t.function(t.variableName), t.function(t.name)], color: "var(--syn-fn)" },
  { tag: [t.special(t.name), t.quote], color: "var(--syn-ident)" },
  { tag: [t.operator, t.punctuation, t.separator, t.bracket], color: "var(--syn-punct)" },
]);

// ---- error underline --------------------------------------------------------

export const setError = StateEffect.define<{ from: number; to: number } | null>();
export const errorField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(deco, tr) {
    for (const e of tr.effects) {
      if (e.is(setError)) {
        if (!e.value) return Decoration.none;
        const len = tr.state.doc.length;
        const from = Math.max(0, Math.min(e.value.from, len));
        const to = Math.max(from, Math.min(e.value.to, len));
        if (to === from) return Decoration.none;
        return Decoration.set([Decoration.mark({ class: "cm-error-underline" }).range(from, to)]);
      }
    }
    // Any edit clears the marker.
    return tr.docChanged ? Decoration.none : deco;
  },
  provide: (f) => EditorView.decorations.from(f),
});

/** Highlighting for the connection's dialect (completion comes from sqlAssist). */
export function langExtension(kind: ConnectorKind | undefined): Extension {
  return sqlLanguage(kind);
}

export interface SqlEditorSetup {
  /** The editor's key: editor bridge, query hints, inline AI (⌘I). */
  editorKey: string;
  /** The connection now (read on each completion request; it can change). */
  connectionId: () => string | null | undefined;
  /** The view (format key). */
  view: () => EditorView | null;
  /** Holds the dialect; reconfigured by useDialect. */
  lang: Compartment;
  placeholder: string;
  ariaLabel: string;
  /** Line numbers and the active line (query tabs, not notebook cells). */
  gutters?: boolean;
  /** Editor-specific keys (run, save…), before the shared ones. */
  keys?: readonly KeyBinding[];
  onMenu: (at: { x: number; y: number }) => void;
  onDocChange: (text: string) => void;
  onFocus?: () => void;
}

/** The extensions of a SQL editor (everything but the document). */
export function sqlEditorExtensions(o: SqlEditorSetup): Extension[] {
  const kindNow = () => {
    const id = o.connectionId();
    return useStore.getState().connections.find((c) => c.id === id)?.config.kind;
  };
  return [
    o.gutters ? [lineNumbers(), highlightActiveLineGutter()] : [],
    sqlFolding(),
    history(),
    drawSelection(),
    indentOnInput(),
    bracketMatching(),
    closeBrackets(),
    // Keywords for the clause, the connection's tables and columns.
    sqlAssist(o.connectionId),
    // Inline AI suggestions (Tab accepts), when turned on in Settings → Editor.
    ghostCompletion(o.connectionId),
    // Tips after a run: partition / index / cluster key (underlines + hover).
    queryHints(() => o.editorKey),
    o.gutters ? highlightActiveLine() : [],
    highlightSelectionMatches(),
    syntaxHighlighting(highlight),
    placeholder(o.placeholder),
    errorField,
    paramField,
    o.lang.of(langExtension(undefined)),
    keymap.of([
      ...(o.keys ?? []),
      {
        key: "Mod-i",
        run: () => {
          window.dispatchEvent(new CustomEvent("db:inline-ai", { detail: { key: o.editorKey } }));
          return true;
        },
        preventDefault: true,
      },
      formatKey(o.view, kindNow),
      ...closeBracketsKeymap,
      ...defaultKeymap,
      ...searchKeymap,
      ...historyKeymap,
      ...completionKeymap,
      indentWithTab,
    ]),
    editorContextMenu(o.onMenu),
    EditorView.updateListener.of((u) => {
      if (u.docChanged) o.onDocChange(u.state.doc.toString());
    }),
    EditorView.contentAttributes.of({ "aria-label": o.ariaLabel }),
    EditorView.domEventHandlers({
      focus: () => {
        editorBridge.setFocused(o.editorKey);
        o.onFocus?.();
      },
    }),
  ];
}

/** Keep the editor's dialect on the connection's, and prefetch its schemas for completion. */
export function useDialect(view: RefObject<EditorView | null>, lang: Compartment, conn: ConnectionView | undefined) {
  // `conn` is a new object on every connections refresh: key on its id and kind.
  const prefetch = useEvent(() => {
    if (conn && canFetchMetadata(conn)) useStore.getState().loadSchemas(conn.id).catch(() => {});
  });
  const kind = conn?.config.kind;
  const connId = conn?.id;
  useEffect(() => {
    view.current?.dispatch({ effects: lang.reconfigure(langExtension(kind)) });
    prefetch();
  }, [view, lang, kind, connId, prefetch]);
}

/**
 * Highlight `:name` parameters (debounced; the backend finds them with the
 * dialect's rules). `onSpans` also gets them (the query tab lists them).
 */
export function useParamSpans(
  view: RefObject<EditorView | null>,
  sql: string,
  kind: ConnectorKind | undefined,
  onSpans?: (spans: ParamSpan[]) => void,
) {
  const report = useEvent((spans: ParamSpan[]) => onSpans?.(spans));
  useEffect(() => {
    if (!isTauri()) return;
    const t = setTimeout(() => {
      const found = sql.includes(":") || sql.includes("&") ? api.sqlParameters(kind ?? "postgres", sql) : Promise.resolve([]);
      found
        .then((spans) => {
          const v = view.current;
          // Typed meanwhile: the next run of this effect will catch up.
          if (!v || v.state.doc.toString() !== sql) return;
          v.dispatch({ effects: setParamSpans.of(spans) });
          report(spans);
        })
        .catch(() => {});
    }, 200);
    return () => clearTimeout(t);
  }, [view, sql, kind, report]);
}
