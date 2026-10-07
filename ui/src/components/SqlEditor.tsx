import { useEffect, useRef, useState } from "react";
import { closeBrackets, closeBracketsKeymap, completionKeymap } from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { HighlightStyle, bracketMatching, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { Compartment, EditorState, StateEffect, StateField, type Extension } from "@codemirror/state";
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
} from "@codemirror/view";
import { tags as t } from "@lezer/highlight";
import type { ConnectorKind } from "../lib/types";
import { useStore } from "../store";
import { editorBridge } from "../editorBridge";
import { canFetchMetadata, sqlAssist, sqlLanguage } from "./sqlAssist";
import { queryHints } from "./queryHintsExt";
import { SqlContextMenu, formatKey } from "./SqlContextMenu";

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

// ---- editor -----------------------------------------------------------------

/** Highlighting for the connection's dialect (completion comes from sqlAssist). */
export function langExtension(kind: ConnectorKind | undefined): Extension {
  return sqlLanguage(kind);
}

export interface EditorHandle {
  run: (mode: "statement" | "all") => void;
}

export function SqlEditor({ tabId, visible }: { tabId: string; visible: boolean }) {
  const host = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const lang = useRef(new Compartment());
  const tab = useStore((s) => s.tabs.find((x) => x.id === tabId));
  const conn = useStore((s) => s.connections.find((c) => c.id === tab?.connection_id));
  const errorRange = useStore((s) => s.runs[tabId]?.errorRange);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);

  // Create the view once per tab.
  useEffect(() => {
    if (!host.current) return;
    const run = (mode: "statement" | "all") => (view: EditorView) => {
      const sel = view.state.selection.main;
      void useStore.getState().runTab(tabId, mode, {
        doc: view.state.doc.toString(),
        selFrom: sel.from,
        selTo: sel.to,
        cursor: sel.head,
      });
      return true;
    };
    const kindNow = () => {
      const st = useStore.getState();
      const cid = st.tabs.find((x) => x.id === tabId)?.connection_id;
      return st.connections.find((c) => c.id === cid)?.config.kind;
    };
    const initial = useStore.getState().tabs.find((x) => x.id === tabId)?.sql ?? "";
    const view = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: initial,
        extensions: [
          lineNumbers(),
          highlightActiveLineGutter(),
          history(),
          drawSelection(),
          indentOnInput(),
          bracketMatching(),
          closeBrackets(),
          // Keywords for the clause, the connection's tables and columns.
          sqlAssist(() => useStore.getState().tabs.find((x) => x.id === tabId)?.connection_id),
          // Tips after a run: partition / index / cluster key (underlines + hover).
          queryHints(() => tabId),
          highlightActiveLine(),
          highlightSelectionMatches(),
          syntaxHighlighting(highlight),
          placeholder("Write SQL…  ⌘↵ run statement · ⇧⌘↵ run all"),
          errorField,
          lang.current.of(langExtension(undefined)),
          keymap.of([
            { key: "Mod-Enter", run: run("statement"), preventDefault: true },
            { key: "Shift-Mod-Enter", run: run("all"), preventDefault: true },
            {
              key: "Mod-i",
              run: () => {
                window.dispatchEvent(new CustomEvent("db:inline-ai", { detail: { key: tabId } }));
                return true;
              },
              preventDefault: true,
            },
            {
              key: "Mod-s",
              run: () => {
                void useStore.getState().saveTabQuery(tabId);
                return true;
              },
              preventDefault: true,
            },
            formatKey(() => viewRef.current, kindNow),
            ...closeBracketsKeymap,
            ...defaultKeymap,
            ...searchKeymap,
            ...historyKeymap,
            ...completionKeymap,
            indentWithTab,
          ]),
          editorContextMenu(setMenu),
          EditorView.updateListener.of((u) => {
            if (!u.docChanged) return;
            const st = useStore.getState();
            const t = st.tabs.find((x) => x.id === tabId);
            st.updateTab(tabId, { sql: u.state.doc.toString(), dirty: !!t?.saved_query_id });
          }),
          EditorView.contentAttributes.of({ "aria-label": "SQL editor" }),
          EditorView.domEventHandlers({ focus: () => editorBridge.setFocused(tabId) }),
        ],
      }),
    });
    viewRef.current = view;
    editorBridge.register(tabId, view);
    return () => {
      editorBridge.unregister(tabId, view);
      view.destroy();
      viewRef.current = null;
    };
  }, [tabId]);

  // Keep the dialect in sync; prefetch schemas for completion.
  useEffect(() => {
    viewRef.current?.dispatch({ effects: lang.current.reconfigure(langExtension(conn?.config.kind)) });
    if (conn && canFetchMetadata(conn)) useStore.getState().loadSchemas(conn.id).catch(() => {});
  }, [conn?.config.kind, conn?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  // External document changes (e.g. opening a saved query into this tab).
  useEffect(() => {
    const v = viewRef.current;
    if (!v || tab === undefined) return;
    const cur = v.state.doc.toString();
    if (tab.sql !== cur) {
      v.dispatch({ changes: { from: 0, to: cur.length, insert: tab.sql } });
    }
  }, [tab?.sql]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    viewRef.current?.dispatch({ effects: setError.of(errorRange ?? null) });
    if (errorRange && viewRef.current) {
      viewRef.current.dispatch({
        effects: EditorView.scrollIntoView(errorRange.from, { y: "center" }),
      });
    }
  }, [errorRange]);

  useEffect(() => {
    if (visible) {
      viewRef.current?.requestMeasure();
      viewRef.current?.focus();
    }
  }, [visible]);

  return (
    <>
      <div ref={host} className="h-full" style={{ display: visible ? "block" : "none" }} />
      {menu && viewRef.current && <SqlContextMenu view={viewRef.current} kind={conn?.config.kind} at={menu} onClose={() => setMenu(null)} />}
    </>
  );
}
