// Opening the tabs that are loaded on first use (lineage, schema compare,
// ER diagram): kept apart from their components so that menus and editors
// can open them without loading the components up front.
import { traitsOf } from "../lib/sourceTraits";
import type { CompareSpec } from "../lib/types";
import { useStore, type CompareRef, type DiffRef } from "../store";
import { editorBridge } from "../editorBridge";

/**
 * Open (or refresh) the lineage tab of an editor: its selection, else the
 * whole script. `editorKey` is a query tab id or a notebook cell key.
 */
export function openLineage(editorKey: string, connectionId?: string | null) {
  const st = useStore.getState();
  const view = editorBridge.get(editorKey);
  const srcTab = st.tabs.find((t) => t.id === editorKey);
  const nbTab = editorKey.startsWith("nb:") ? st.tabs.find((t) => t.notebook_id && editorKey.startsWith(`nb:${t.notebook_id}:`)) : undefined;
  const doc = view?.state.doc.toString() ?? srcTab?.sql ?? "";
  const sel = view?.state.selection.main;
  const whole = !sel || sel.empty;
  const sql = whole ? doc : doc.slice(sel.from, sel.to);
  if (!sql.trim()) return st.toast("Nothing to trace: write a query first", "info");
  const conn = connectionId !== undefined ? connectionId : (srcTab?.connection_id ?? nbTab?.connection_id ?? null);
  const kind = st.connections.find((c) => c.id === conn)?.config.kind;
  if (kind && traitsOf(kind).language !== "sql") return st.toast(`Lineage traces SQL; ${traitsOf(kind).name} queries aren't supported`, "info");
  const lineage = { source: editorKey, base: whole ? 0 : sel.from, whole };
  const existing = st.tabs.find((t) => t.lineage?.source === editorKey && !t.lineage.column);
  if (existing) {
    st.updateTab(existing.id, { sql, lineage, connection_id: conn });
    return st.setActiveTab(existing.id);
  }
  const title = `Lineage · ${(srcTab ?? nbTab)?.title ?? "SQL"}`;
  st.newTab({ title, sql, connection_id: conn, lineage });
}

type Side = NonNullable<CompareRef["source"]>;

/** Open a schema compare tab (`source` filled in when started from a schema). */
export function openCompare(source?: Side) {
  const st = useStore.getState();
  st.newTab({ title: "Compare schemas", sql: "", compare: { source: source ?? null, target: null }, connection_id: source?.connection_id ?? null });
}

/** Open a tab comparing the data of two outputs. */
export function openOutputCompare(spec: CompareSpec, title: string, sources?: DiffRef["sources"]) {
  useStore.getState().newTab({ title, sql: "", connection_id: null, diff: { spec, sources } });
}

/** Open (or show the open) ER diagram of a schema. */
export function openErDiagram(connectionId: string, schema: string, label: string) {
  const st = useStore.getState();
  const open = st.tabs.find((t) => t.er && t.connection_id === connectionId && t.er.schema === schema);
  if (open) return st.setActiveTab(open.id);
  st.newTab({ title: `ER · ${label}`, sql: "", connection_id: connectionId, er: { schema, label } });
}
