import { useEffect, useRef, useState } from "react";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { useStore } from "../../store";
import { editorBridge } from "../../editorBridge";
import { setError, sqlEditorExtensions, useDialect, useParamSpans } from "../sqlEditorSetup";
import { SqlContextMenu } from "../SqlContextMenu";
import { openLineage } from "../LineageTab";
import { ResizeHandle } from "../ResizeHandle";
import { EDITOR_MAX, EDITOR_MIN } from "../../lib/resize";
import { kbd } from "../../lib/platform";

// ------------------------------------------------------------------ editors

export function CellEditor({
  editorKey,
  source,
  connectionId,
  error,
  height,
  onResize,
  onChange,
  onRun,
  onRunAdvance,
  onFocus,
}: {
  editorKey: string;
  source: string;
  connectionId: string | null;
  error?: { from: number; to: number };
  /** Fixed height in px (null = grows with the SQL). */
  height: number | null;
  onResize: (h: number | null) => void;
  onChange: (s: string) => void;
  onRun: () => void;
  onRunAdvance: () => void;
  onFocus: () => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const lang = useRef(new Compartment());
  const cb = useRef({ onChange, onRun, onRunAdvance, onFocus });
  cb.current = { onChange, onRun, onRunAdvance, onFocus };
  const conn = useStore((s) =>
    s.connections.find((c) => c.id === connectionId),
  );
  // Read on each completion request (the cell's connection can change).
  const connRef = useRef(connectionId);
  connRef.current = connectionId;
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);

  useEffect(() => {
    if (!host.current) return;
    const v = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: source,
        extensions: [
          sqlEditorExtensions({
            editorKey,
            connectionId: () => connRef.current,
            view: () => view.current,
            lang: lang.current,
            placeholder: kbd("SQL…  ⌘↵ run · ⇧↵ run & next · ⌘I ask AI"),
            ariaLabel: "SQL cell",
            keys: [
              { key: "Mod-Enter", run: () => (cb.current.onRun(), true), preventDefault: true },
              { key: "Shift-Enter", run: () => (cb.current.onRunAdvance(), true), preventDefault: true },
            ],
            onMenu: setMenu,
            onDocChange: (text) => cb.current.onChange(text),
            onFocus: () => cb.current.onFocus(),
          }),
          EditorView.theme({
            "&": { minHeight: "38px" },
            ".cm-scroller": { fontSize: "calc(var(--editor-font-size, 13.5px) - 0.5px)" },
          }),
        ],
      }),
    });
    view.current = v;
    editorBridge.register(editorKey, v);
    return () => {
      editorBridge.unregister(editorKey, v);
      v.destroy();
      view.current = null;
    };
    // The editor is created once per cell; later changes are dispatched into it.
    // eslint-disable-next-line react-hooks/exhaustive-deps -- created once per editorKey
  }, [editorKey]);

  useDialect(view, lang.current, conn);

  // Highlight `:name` parameters (values are set in the notebook's parameter bar).
  useParamSpans(view, source, conn?.config.kind);

  // External changes (AI edits applied through the bridge already go through the view).
  useEffect(() => {
    const v = view.current;
    if (v && source !== v.state.doc.toString())
      v.dispatch({
        changes: { from: 0, to: v.state.doc.length, insert: source },
      });
  }, [source]);

  useEffect(() => {
    view.current?.dispatch({ effects: setError.of(error ?? null) });
  }, [error]);

  // While dragging the height is local; it is saved on release.
  const [dragH, setDragH] = useState<number | null>(null);
  const h = dragH ?? height;
  useEffect(() => {
    view.current?.requestMeasure();
  }, [h]);
  return (
    <div>
      <div
        ref={host}
        className={`nb-cell overflow-hidden rounded-md border border-line/60 bg-panel-2/40 ${h ? "nb-cell-fixed" : ""}`}
        style={h ? { height: h } : undefined}
      />
      {menu && view.current && <SqlContextMenu view={view.current} kind={conn?.config.kind} at={menu} onClose={() => setMenu(null)} onLineage={() => openLineage(editorKey, connectionId ?? undefined)} />}
      <ResizeHandle
        label="Editor height"
        min={EDITOR_MIN}
        max={EDITOR_MAX}
        height={() =>
          host.current?.getBoundingClientRect().height ?? EDITOR_MIN
        }
        onChange={setDragH}
        onCommit={(v) => {
          setDragH(null);
          onResize(v);
        }}
        onReset={() => {
          setDragH(null);
          onResize(null);
        }}
      />
    </div>
  );
}
