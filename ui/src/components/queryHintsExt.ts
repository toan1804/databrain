// Editor query tips: after a query runs, underline filters that defeat
// partitions / indexes / cluster keys and show table layouts on hover (see
// queryTips.ts). The layout cache below also feeds completion badges.
import { StateEffect, StateField, type Extension } from "@codemirror/state";
import { Decoration, EditorView, ViewPlugin, hoverTooltip, type DecorationSet } from "@codemirror/view";
import { api } from "../lib/api";
import { layoutSummary } from "../lib/queryHints";
import type { TableLayout } from "../lib/types";
import type { RunTips } from "../queryTips";
import { useStore } from "../store";

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

// ---- run tips -------------------------------------------------------------------

export interface DocHint {
  from: number;
  to: number;
  level: "warn" | "info";
  message: string;
}

interface Analysis {
  hints: DocHint[];
  /** Table references with a known layout (for hover cards). */
  tables: { from: number; to: number; title: string; layout: TableLayout }[];
}

/** Tips of the last run, limited to statements the editor still shows unchanged. */
function tipsForView(view: EditorView, tips: RunTips | undefined): Analysis {
  if (!tips) return { hints: [], tables: [] };
  const doc = view.state.doc;
  const intact = (pos: number) =>
    tips.statements.some((st) => pos >= st.start && pos <= st.start + st.sql.length && st.start + st.sql.length <= doc.length && doc.sliceString(st.start, st.start + st.sql.length) === st.sql);
  return {
    hints: tips.tips.filter((t) => intact(t.from)).map((t) => ({ from: t.from, to: t.to, level: t.level, message: t.message })),
    tables: tips.tables.filter((t) => intact(t.from)),
  };
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

function hintPlugin(getKey: () => string | null | undefined) {
  return ViewPlugin.fromClass(
    class {
      unsub: () => void;
      last: RunTips | undefined | null = null;
      constructor(readonly view: EditorView) {
        // Tips arrive after a run finishes; a new run clears them.
        const sync = () => {
          const key = getKey();
          const tips = key ? useStore.getState().runs[key]?.tips : undefined;
          if (tips === this.last) return;
          this.last = tips;
          queueMicrotask(() => this.view.dispatch({ effects: setAnalysis.of(tipsForView(this.view, tips)) }));
        };
        this.unsub = useStore.subscribe(sync);
        sync();
      }
      destroy() {
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

/**
 * Query tips in the editor of run key `getKey` (tab id or notebook cell key):
 * after a run, filters that defeat partitions / indexes / cluster keys are
 * underlined with the reason on hover, and table names show their layout.
 */
export function queryHints(getKey: () => string | null | undefined): Extension {
  return [analysisField, hintPlugin(getKey), hover];
}
