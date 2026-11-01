import { draftOf, setDraft, useDrafts } from "../drafts";
import { memo, useEffect, useRef, useState } from "react";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { useStore } from "../store";
import { editorBridge } from "../editorBridge";
import { SqlContextMenu } from "./SqlContextMenu";
import { openLineage } from "./LineageTab";
import { paramNames, textualNames } from "./paramsExt";
import { api, isTauri } from "../lib/api";
import { statementTarget } from "../lib/statementNav";
import { kbd } from "../lib/platform";
import { setError, sqlEditorExtensions, useDialect, useParamSpans } from "./sqlEditorSetup";

export interface EditorHandle {
  run: (mode: "statement" | "all") => void;
}

/** One per query tab, hidden ones too: memoized, so only the tab whose props change renders. */
export const SqlEditor = memo(function SqlEditor({ tabId, visible }: { tabId: string; visible: boolean }) {
  const host = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const lang = useRef(new Compartment());
  const tab = useStore((s) => s.tabs.find((x) => x.id === tabId));
  // What is typed (the tab's text follows after a pause).
  const draft = useDrafts((s) => s.text[tabId]);
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
    // ⌘⌥↓ / ⌘⌥↑: cursor to the next / previous statement (as the splitter sees them).
    const gotoStatement = (dir: 1 | -1) => (view: EditorView) => {
      if (!isTauri()) return false;
      const doc = view.state.doc.toString();
      const cursor = view.state.selection.main.head;
      void api.statementSpans(kindNow() ?? "postgres", doc).then((spans) => {
        if (view.state.doc.toString() !== doc) return; // edited meanwhile
        const to = statementTarget(spans, cursor, dir);
        if (to !== null) view.dispatch({ selection: { anchor: to }, scrollIntoView: true });
      });
      return true;
    };
    const initial = useStore.getState().tabs.find((x) => x.id === tabId)?.sql ?? "";
    const view = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: initial,
        extensions: sqlEditorExtensions({
          editorKey: tabId,
          connectionId: () => useStore.getState().tabs.find((x) => x.id === tabId)?.connection_id,
          view: () => viewRef.current,
          lang: lang.current,
          placeholder: kbd("Write SQL…  ⌘↵ run statement · ⇧⌘↵ run all"),
          ariaLabel: "SQL editor",
          gutters: true,
          keys: [
            { key: "Mod-Enter", run: run("statement"), preventDefault: true },
            { key: "Shift-Mod-Enter", run: run("all"), preventDefault: true },
            { key: "Mod-Alt-ArrowDown", run: gotoStatement(1), preventDefault: true },
            { key: "Mod-Alt-ArrowUp", run: gotoStatement(-1), preventDefault: true },
            {
              key: "Mod-s",
              run: () => {
                const st = useStore.getState();
                if (st.tabs.find((t) => t.id === tabId)?.file_path) void st.saveTabFile(tabId);
                else void st.saveTabQuery(tabId);
                return true;
              },
              preventDefault: true,
            },
            {
              key: "Shift-Mod-s",
              run: () => {
                void useStore.getState().saveTabFile(tabId, true);
                return true;
              },
              preventDefault: true,
            },
          ],
          onMenu: setMenu,
          onDocChange: (text) => {
            const st = useStore.getState();
            const t = st.tabs.find((x) => x.id === tabId);
            // The text goes to the drafts, not the tabs: typing doesn't
            // re-render what lists tabs. The tab changes only when "unsaved" flips.
            setDraft(tabId, text);
            const dirty = t?.file_text !== undefined ? text !== t.file_text : !!t?.saved_query_id;
            if (t && !!t.dirty !== dirty) st.updateTab(tabId, { dirty });
          },
        }),
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
  useDialect(viewRef, lang.current, conn);

  // External document changes (e.g. opening a saved query into this tab).
  const sql = tab?.sql;
  useEffect(() => {
    const v = viewRef.current;
    if (!v || sql === undefined) return;
    // Newer text is still being typed: this is an older flush, not an outside change.
    if (draftOf(tabId) !== undefined) return;
    const cur = v.state.doc.toString();
    if (sql !== cur) {
      v.dispatch({ changes: { from: 0, to: cur.length, insert: sql } });
    }
  }, [sql, tabId]);

  // `:name` parameters: highlight them and list them in the parameter bar (debounced).
  useParamSpans(viewRef, draft ?? tab?.sql ?? "", conn?.config.kind, (spans) => useStore.getState().setParamNames(tabId, paramNames(spans), textualNames(spans)));

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
      {menu && viewRef.current && <SqlContextMenu view={viewRef.current} kind={conn?.config.kind} at={menu} onClose={() => setMenu(null)} onLineage={() => openLineage(tabId)} />}
    </>
  );
});
