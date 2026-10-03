// Editor query hints: underline filters that defeat partitions / indexes /
// cluster keys, and show a table's layout on hover. Layouts come from the
// backend (`table_layout`) and are cached per connection for a few minutes.
import { StateEffect, StateField, type Extension } from "@codemirror/state";
import { Decoration, EditorView, ViewPlugin, hoverTooltip, type DecorationSet, type ViewUpdate } from "@codemirror/view";
import { api } from "../lib/api";
import { analyzeStatement, layoutSummary, refsWithPos, type Hint, type LayoutRef, type RefAt } from "../lib/queryHints";
import { blankLiterals, resolveTable } from "../lib/sqlComplete";
import type { TableLayout } from "../lib/types";
import { useStore } from "../store";
import { canFetchMetadata, storeProvider } from "./sqlAssist";

// ---- layout cache -------------------------------------------------------------

const TTL = 5 * 60_000;
const cache = new Map<string, { at: number; layout: TableLayout | null }>();
const pending = new Map<string, Promise<TableLayout | null>>();
const listeners = new Set<() => void>();

const key = (conn: string, schema: string, name: string) => `${conn}|${schema}|${name}`.toLowerCase();

/** Cached layout (undefined = not loaded yet; null = unavailable). */
export function cachedLayout(conn: string, schema: string, name: string): TableLayout | null | undefined {
  const hit = cache.get(key(conn, schema, name));
  return hit && Date.now() - hit.at < TTL ? hit.layout : undefined;
}

/** Load a layout once (shared by concurrent callers); listeners re-run on arrival. */
export function loadLayout(conn: string, schema: string, name: string): Promise<TableLayout | null> {
  const k = key(conn, schema, name);
  const hit = cachedLayout(conn, schema, name);
  if (hit !== undefined) return Promise.resolve(hit);
  const p0 = pending.get(k);
  if (p0) return p0;
  const p = api
    .tableLayout(conn, schema, name)
    .catch(() => null)
    .then((layout) => {
      cache.set(k, { at: Date.now(), layout });
      pending.delete(k);
      if (cache.size > 2000) cache.delete(cache.keys().next().value!);
      for (const f of listeners) f();
      return layout;
    });
  pending.set(k, p);
  return p;
}

/** Forget cached layouts of a connection (e.g. after DDL or reconnect). */
export function forgetLayouts(conn: string) {
  const prefix = `${conn}|`.toLowerCase();
  for (const k of [...cache.keys()]) if (k.startsWith(prefix)) cache.delete(k);
}

// ---- analysis -----------------------------------------------------------------

export interface DocHint extends Hint {
  table?: string;
}

interface Analysis {
  hints: DocHint[];
  /** Table references with a known layout (for hover cards). */
  tables: { from: number; to: number; title: string; layout: TableLayout }[];
}

const MAX_DOC = 100_000;

/** Hints and layout cards for a whole document (exported for tests). */
export async function analyze(doc: string, connId: string): Promise<Analysis> {
  const out: Analysis = { hints: [], tables: [] };
  const st = useStore.getState();
  const conn = st.connections.find((c) => c.id === connId);
  const p = storeProvider(connId);
  if (!conn || !p || doc.length > MAX_DOC) return out;
  // Only look up layouts when that doesn't open a sign-in prompt.
  if (!canFetchMetadata(conn)) return out;
  const clean = blankLiterals(doc);
  let start = 0;
  const stmts: [number, number][] = [];
  for (let i = 0; i <= clean.length; i++) {
    if (i === clean.length || clean[i] === ";") {
      if (clean.slice(start, i).trim()) stmts.push([start, i]);
      start = i + 1;
    }
  }
  for (const [a, b] of stmts.slice(0, 50)) {
    const text = doc.slice(a, b);
    const refs = refsWithPos(text).filter((r) => !(p.kind === "duckdb" && r.parts[0]?.toLowerCase() === "results"));
    const known: LayoutRef[] = [];
    await Promise.all(
      refs.map(async (ref: RefAt) => {
        const t = await resolveTable(p, ref.parts);
        if (!t) return;
        const layout = await loadLayout(connId, t.schema, t.name);
        if (!layout) return;
        known.push({ ref, layout });
        out.tables.push({ from: a + ref.from, to: a + ref.to, title: ref.parts.join("."), layout });
      }),
    );
    known.sort((x, y) => x.ref.from - y.ref.from);
    for (const h of analyzeStatement(text, known, p.kind)) out.hints.push({ ...h, from: a + h.from, to: a + h.to });
  }
  return out;
}

// ---- editor state -------------------------------------------------------------

const setAnalysis = StateEffect.define<Analysis>();

const analysisField = StateField.define<{ a: Analysis; deco: DecorationSet }>({
  create: () => ({ a: { hints: [], tables: [] }, deco: Decoration.none }),
  update(v, tr) {
    for (const e of tr.effects) {
      if (e.is(setAnalysis)) {
        const len = tr.state.doc.length;
        const marks = e.value.hints
          .filter((h) => h.to > h.from && h.to <= len)
          .sort((x, y) => x.from - y.from || x.to - y.to)
          .map((h) => Decoration.mark({ class: h.level === "warn" ? "cm-hint-warn" : "cm-hint-info" }).range(h.from, h.to));
        return { a: e.value, deco: Decoration.set(marks, true) };
      }
    }
    if (!tr.docChanged) return v;
    // Keep marks in place while typing; positions are remapped until the next analysis.
    const map = (x: { from: number; to: number }) => ({ from: tr.changes.mapPos(x.from, 1), to: tr.changes.mapPos(x.to, -1) });
    return {
      a: { hints: v.a.hints.map((h) => ({ ...h, ...map(h) })), tables: v.a.tables.map((t) => ({ ...t, ...map(t) })) },
      deco: v.deco.map(tr.changes),
    };
  },
  provide: (f) => EditorView.decorations.from(f, (v) => v.deco),
});

function hintPlugin(getConnId: () => string | null | undefined) {
  return ViewPlugin.fromClass(
    class {
      timer: ReturnType<typeof setTimeout> | undefined;
      run = 0;
      unsub: () => void;
      lastConn: string | undefined;
      constructor(readonly view: EditorView) {
        this.schedule(50);
        listeners.add(this.onLayout);
        // Re-analyze when the editor's connection changes or (dis)connects.
        const sig = () => {
          const id = getConnId();
          const c = id ? useStore.getState().connections.find((x) => x.id === id) : undefined;
          return `${id}|${c?.connected}`;
        };
        this.lastConn = sig();
        this.unsub = useStore.subscribe(() => {
          const s = sig();
          if (s !== this.lastConn) {
            this.lastConn = s;
            this.schedule(100);
          }
        });
      }
      onLayout = () => this.schedule(100);
      update(u: ViewUpdate) {
        if (u.docChanged) this.schedule(600);
      }
      schedule(ms: number) {
        clearTimeout(this.timer);
        this.timer = setTimeout(() => void this.analyze(), ms);
      }
      async analyze() {
        const id = getConnId();
        const run = ++this.run;
        const doc = this.view.state.doc.toString();
        const a = id ? await analyze(doc, id) : { hints: [], tables: [] };
        // Stale (the text changed meanwhile) or destroyed.
        if (run !== this.run || this.view.state.doc.toString() !== doc) return;
        this.view.dispatch({ effects: setAnalysis.of(a) });
      }
      destroy() {
        clearTimeout(this.timer);
        this.run++;
        listeners.delete(this.onLayout);
        this.unsub();
      }
    },
  );
}

function card(title: string, lines: string[], hints: DocHint[]): HTMLElement {
  const el = document.createElement("div");
  el.className = "cm-hint-card";
  for (const h of hints) {
    const p = document.createElement("div");
    p.className = h.level === "warn" ? "cm-hint-card-warn" : "cm-hint-card-info";
    p.textContent = h.message;
    el.appendChild(p);
  }
  if (lines.length) {
    const t = document.createElement("div");
    t.className = "cm-hint-card-title";
    t.textContent = title;
    el.appendChild(t);
    for (const l of lines) {
      const d = document.createElement("div");
      d.textContent = l;
      el.appendChild(d);
    }
  }
  return el;
}

const hover = hoverTooltip(
  (view, pos) => {
    const { a } = view.state.field(analysisField);
    const hs = a.hints.filter((h) => pos >= h.from && pos <= h.to);
    const t = a.tables.find((x) => pos >= x.from && pos <= x.to);
    const lines = t ? layoutSummary(t.layout) : [];
    if (!hs.length && !lines.length) return null;
    const from = Math.min(...hs.map((h) => h.from), t?.from ?? Infinity);
    const to = Math.max(...hs.map((h) => h.to), t?.to ?? -Infinity);
    return { pos: from, end: to, above: true, create: () => ({ dom: card(t?.title ?? "", lines, hs) }) };
  },
  { hoverTime: 250 },
);

/** Query hints for the connection returned by `getConnId` (read on each analysis). */
export function queryHints(getConnId: () => string | null | undefined): Extension {
  return [analysisField, hintPlugin(getConnId), hover];
}
