import { useCallback, useEffect, useRef, useState } from "react";
import { EditorState } from "@codemirror/state";
import { EditorView, lineNumbers } from "@codemirror/view";
import { syntaxHighlighting } from "@codemirror/language";
import { highlightSelectionMatches, search, searchKeymap } from "@codemirror/search";
import { keymap } from "@codemirror/view";
import { AlertCircle, ClipboardCopy, FileCode2, Loader2, Lock, RefreshCw } from "lucide-react";
import { api, toError } from "../lib/api";
import { tablePath } from "../lib/catalog";
import { useStore, type DdlRef } from "../store";
import { highlight, langExtension } from "./SqlEditor";
import { copyText } from "./CatalogMenus";
import { ConnDot } from "./ui";

type State = { status: "loading" } | { status: "ready"; ddl: string } | { status: "empty" } | { status: "error"; message: string };

/**
 * Read-only DDL of an object (explorer "Show DDL"). Opens at once and loads
 * from the server meanwhile; Copy and "Open in editor" act on what is shown.
 */
export function DdlTab({ tabId, reference, visible }: { tabId: string; reference: DdlRef; visible: boolean }) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const conn = useStore((s) => s.connections.find((c) => c.id === tab?.connection_id));
  const [state, setState] = useState<State>({ status: "loading" });
  const seq = useRef(0);

  const load = useCallback(async () => {
    const my = ++seq.current;
    setState({ status: "loading" });
    if (!conn) return setState({ status: "error", message: "The connection of this tab no longer exists." });
    try {
      const ddl = await api.objectDdl(conn.id, reference.schema, reference.name, reference.kind);
      if (my !== seq.current) return;
      setState(ddl?.trim() ? { status: "ready", ddl: ddl.endsWith("\n") ? ddl : `${ddl}\n` } : { status: "empty" });
    } catch (e) {
      if (my === seq.current) setState({ status: "error", message: toError(e).message });
    }
    // Reload only when the object or connection changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [conn?.id, reference.schema, reference.name, reference.kind]);

  useEffect(() => {
    void load();
  }, [load]);

  const ddl = state.status === "ready" ? state.ddl : null;
  const path = conn ? tablePath(conn.config.kind, reference.schema, reference.name) : reference.name;
  const openInEditor = () => {
    if (ddl) useStore.getState().newTab({ title: `${reference.name} DDL (edit)`, sql: ddl, connection_id: conn?.id ?? null });
  };

  return (
    <div className="h-full min-h-0 flex-col" style={{ display: visible ? "flex" : "none" }}>
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-line px-3 text-[12.5px]">
        <FileCode2 size={14} className="shrink-0 text-muted" />
        <span className="min-w-0 truncate font-mono" title={path}>
          {path}
        </span>
        <span className="shrink-0 text-muted">· {reference.kind.replace("_", " ")}</span>
        {conn && (
          <span className="flex shrink-0 items-center gap-1 text-muted">
            · <ConnDot color={conn.color} connected={conn.connected} /> {conn.name}
          </span>
        )}
        <span className="flex shrink-0 items-center gap-1 rounded bg-panel-2 px-1.5 py-0.5 text-[11px] text-muted" title="This tab only shows the DDL; it can't be edited or run">
          <Lock size={10} /> read-only
        </span>
        <div className="ml-auto flex items-center gap-1">
          <button className="btn-ghost py-1" onClick={() => ddl && void copyText(ddl, "DDL")} disabled={!ddl} title="Copy the DDL">
            <ClipboardCopy size={13} /> Copy
          </button>
          <button className="btn-ghost py-1" onClick={openInEditor} disabled={!ddl} title="Open a copy in an editable query tab">
            <FileCode2 size={13} /> Open in editor
          </button>
          <button className="btn-ghost py-1" onClick={() => void load()} disabled={state.status === "loading"} title="Load the DDL again from the server">
            <RefreshCw size={13} className={state.status === "loading" ? "animate-spin" : ""} /> Reload
          </button>
        </div>
      </div>
      <div className="min-h-0 flex-1" aria-busy={state.status === "loading"}>
        {state.status === "loading" && (
          <div className="flex h-full items-center justify-center gap-2 text-[12.5px] text-muted" role="status">
            <Loader2 size={15} className="animate-spin text-accent" /> Loading DDL of {reference.name} from {conn?.name ?? "the server"}…
          </div>
        )}
        {state.status === "empty" && (
          <div className="flex h-full items-center justify-center text-[12.5px] text-muted">DDL is not available for {reference.name} on this connection.</div>
        )}
        {state.status === "error" && (
          <div className="flex h-full flex-col items-center justify-center gap-2 px-6 text-center text-[12.5px]" role="alert">
            <span className="flex items-center gap-1.5 text-danger">
              <AlertCircle size={14} /> Could not load the DDL
            </span>
            <span className="max-w-xl break-words text-muted">{state.message}</span>
            <button className="btn-ghost border border-line py-1" onClick={() => void load()}>
              <RefreshCw size={12} /> Try again
            </button>
          </div>
        )}
        {ddl !== null && <DdlView text={ddl} kind={conn?.config.kind} />}
      </div>
    </div>
  );
}

/** CodeMirror in read-only mode: highlighting, selection, copy and ⌘F search, no edits. */
function DdlView({ text, kind }: { text: string; kind: Parameters<typeof langExtension>[0] }) {
  const host = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const view = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: text,
        extensions: [
          lineNumbers(),
          langExtension(kind),
          syntaxHighlighting(highlight),
          highlightSelectionMatches(),
          search({ top: true }),
          keymap.of(searchKeymap),
          EditorState.readOnly.of(true),
          EditorView.editable.of(false),
          // Focusable (selection, ⌘C, ⌘F) though not editable.
          EditorView.contentAttributes.of({ tabindex: "0", "aria-label": "DDL (read-only)", "aria-readonly": "true" }),
          EditorView.theme({ "&": { height: "100%" }, ".cm-scroller": { overflow: "auto" } }),
        ],
      }),
    });
    return () => view.destroy();
  }, [text, kind]);
  return <div ref={host} className="h-full min-h-0" />;
}
