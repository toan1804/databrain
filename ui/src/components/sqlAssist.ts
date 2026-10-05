// CodeMirror adapter for lib/sqlComplete: SQL highlighting per dialect plus
// completion from the connection's metadata (store caches + server search).
import { autocompletion, startCompletion, type Completion, type CompletionContext, type CompletionResult } from "@codemirror/autocomplete";
import { MSSQL, MySQL, PLSQL, PostgreSQL, SQLite, StandardSQL, type SQLDialect } from "@codemirror/lang-sql";
import { LanguageSupport } from "@codemirror/language";
import type { Extension } from "@codemirror/state";
import type { EditorView } from "@codemirror/view";
import { api } from "../lib/api";
import { completeSql, type MetaProvider, type SqlOption } from "../lib/sqlComplete";
import type { ConnectorKind, DbObject } from "../lib/types";
import { columnRoles } from "../lib/queryHints";
import { useStore } from "../store";
import { cachedLayout, loadLayout } from "./queryHintsExt";

const DIALECTS: Record<ConnectorKind, SQLDialect> = {
  postgres: PostgreSQL,
  mysql: MySQL,
  sqlite: SQLite,
  mssql: MSSQL,
  oracle: PLSQL,
  snowflake: StandardSQL,
  databricks: MySQL, // backtick identifiers
  bigquery: MySQL, // backtick identifiers
  duckdb: PostgreSQL,
};

/** Highlighting only: lang-sql's own completion (every keyword of the dialect) is left out. */
export function sqlLanguage(kind: ConnectorKind | undefined): Extension {
  return new LanguageSupport((kind ? DIALECTS[kind] : PostgreSQL).language);
}

// Completion metadata is fetched on demand, filtered by what was typed (like
// DBeaver / dbx): never a whole schema or catalog. Each lookup answers from
// memory or the local knowledge index at once; the server is asked too, and
// if it is slower than BUDGET_MS the list shows what is known and refreshes
// itself when the server answers.
const BUDGET_MS = 150;
const SEARCH_LIMIT = 200;
const SEARCH_TTL = 60_000;

interface Search {
  at: number;
  p: Promise<DbObject[]>;
  /** Set once resolved. */
  value?: DbObject[];
}
const searches = new Map<string, Search>();
const loading = new Set<string>();
/** Column loads in flight (one describe per table, however fast the typing). */
const columnLoads = new Map<string, Promise<string[] | undefined>>();

function remember(key: string, p: Promise<DbObject[]>): Search {
  const e: Search = { at: Date.now(), p };
  p.then((v) => (e.value = v)).catch(() => {});
  searches.set(key, e);
  if (searches.size > 400) searches.delete(searches.keys().next().value!);
  return e;
}

/**
 * A finished search for a shorter prefix that returned fewer than the limit
 * already contains every match for the longer one: filter it locally.
 */
function narrowed(scope: string, prefix: string): DbObject[] | undefined {
  const q = prefix.toLowerCase();
  for (let n = q.length - 1; n >= 1; n--) {
    const e = searches.get(`${scope}|${q.slice(0, n)}`);
    if (e?.value && Date.now() - e.at < SEARCH_TTL && e.value.length < SEARCH_LIMIT) {
      return e.value.filter((o) => o.name.toLowerCase().includes(q));
    }
  }
  return undefined;
}

function cached(key: string, f: () => Promise<DbObject[]>): Search {
  const hit = searches.get(key);
  if (hit && Date.now() - hit.at < SEARCH_TTL) return hit;
  return remember(key, f());
}

/** `p` if it settles within the budget, else `fallback` (and `onLate` once it settles). */
function withBudget<T>(p: Promise<T>, fallback: T, onLate?: () => void): Promise<T> {
  let late = false;
  p.then(
    () => late && onLate?.(),
    () => {},
  );
  return Promise.race([p, new Promise<T>((r) => setTimeout(() => ((late = true), r(fallback)), BUDGET_MS))]);
}

function merge(...lists: DbObject[][]): DbObject[] {
  const seen = new Set<string>();
  const out: DbObject[] = [];
  for (const l of lists)
    for (const o of l) {
      const k = `${o.schema}\u0000${o.name}`.toLowerCase();
      if (!seen.has(k)) {
        seen.add(k);
        out.push(o);
      }
    }
  return out;
}

const INTERACTIVE = ["oauth_browser", "device_code", "external_browser"];

// Loaded objects of a connection, flattened and indexed by name. Rebuilt only
// when the store's `objects` map changes (not per keystroke).
const indexCache = new WeakMap<object, Map<string, { all: DbObject[]; byName: Map<string, DbObject[]> }>>();
function cachedIndex(connId: string) {
  const objects = useStore.getState().objects;
  let perMap = indexCache.get(objects);
  if (!perMap) {
    perMap = new Map();
    indexCache.set(objects, perMap);
  }
  let idx = perMap.get(connId);
  if (!idx) {
    const all: DbObject[] = [];
    const byName = new Map<string, DbObject[]>();
    const prefix = `${connId}|`;
    for (const [k, v] of Object.entries(objects)) {
      if (!k.startsWith(prefix)) continue;
      for (const o of v) {
        all.push(o);
        const n = o.name.toLowerCase();
        const list = byName.get(n);
        if (list) list.push(o);
        else byName.set(n, [o]);
      }
    }
    idx = { all, byName };
    perMap.set(connId, idx);
  }
  return idx;
}

/**
 * May completion connect on its own? Yes when already connected or when no
 * interactive sign-in (browser/device code) would pop up while typing.
 */
export function canFetchMetadata(conn: { connected: boolean; config: { auth: { method: string } } }): boolean {
  return conn.connected || !INTERACTIVE.includes(conn.config.auth.method);
}

/**
 * Metadata for completion (loads on demand, filtered by the typed text).
 * `onLate` runs when a server answer arrives after the list was shown.
 */
export function storeProvider(connId: string, onLate?: () => void): MetaProvider | null {
  const st = useStore.getState();
  const conn = st.connections.find((c) => c.id === connId);
  if (!conn) return null;
  const live = canFetchMetadata(conn);
  const once = (key: string, f: () => Promise<unknown>) => {
    if (loading.has(key)) return;
    loading.add(key);
    f()
      .catch(() => {})
      .finally(() => loading.delete(key));
  };
  /** Local index + server, for one schema (or all when null). */
  const search = async (schema: string | null, prefix: string): Promise<DbObject[]> => {
    const scope = `${connId}|${schema ?? "*"}`;
    const q = prefix.toLowerCase();
    const near = narrowed(scope, q);
    if (near) return near;
    const local = cached(`l|${scope}|${q}`, () => api.completeTablesLocal(connId, schema, prefix, SEARCH_LIMIT).catch(() => [] as DbObject[]));
    const localHits = await local.p;
    if (!live) return localHits;
    const remote = cached(`${scope}|${q}`, () => api.completeTables(connId, schema, prefix, SEARCH_LIMIT).catch(() => [] as DbObject[]));
    if (remote.value) return merge(localHits, remote.value);
    return merge(localHits, await withBudget(remote.p, [] as DbObject[], onLate));
  };
  return {
    kind: conn.config.kind,
    schemas: () => {
      const s = useStore.getState().schemas[connId];
      if (!s && live) once(`s|${connId}`, () => useStore.getState().loadSchemas(connId).then(() => onLate?.()));
      return s;
    },
    objects: async (schema, typed) => {
      // Loaded by the explorer: use it. Otherwise search (big schemas are never loaded whole).
      const loaded = useStore.getState().objects[`${connId}|${schema}`];
      if (loaded) return loaded;
      return search(schema, typed);
    },
    cachedObjects: () => cachedIndex(connId).all,
    cachedNamed: (name) => cachedIndex(connId).byName.get(name.toLowerCase()) ?? [],
    searchTables: (prefix) => (prefix ? search(null, prefix) : Promise.resolve([])),
    columns: async (schema, table) => {
      const key = `${connId}|${schema}|${table}`;
      const have = useStore.getState().columns[key];
      if (have) return have.map((c) => c.name);
      const local = await api.completeColumnsLocal(connId, schema, table).catch(() => null);
      if (!live) return local ?? undefined;
      let remote = columnLoads.get(key);
      if (!remote) {
        remote = useStore
          .getState()
          .loadColumns(connId, schema, table)
          .then((c) => c.map((x) => x.name))
          .catch(() => undefined)
          .finally(() => setTimeout(() => columnLoads.delete(key), 5_000));
        columnLoads.set(key, remote);
      }
      // The index answers at once; the live columns refresh the cache.
      if (local?.length) return local;
      return withBudget(remote, undefined, onLate);
    },
    columnRoles: (schema, table) => {
      const l = cachedLayout(connId, schema, table);
      if (l === undefined && live) void loadLayout(connId, schema, table);
      return l ? columnRoles(l) : undefined;
    },
    virtualTables: () =>
      conn.config.kind !== "duckdb"
        ? []
        : useStore
            .getState()
            .outputs.filter((o) => o.state !== "evicted")
            .flatMap((o) => {
              const cols = o.columns.map((c) => c.name);
              const names = [o.handle, ...(o.name ? [o.name] : []), ...(o.version_of ? [`${o.version_of[0]}__${o.version_of[1]}`] : [])];
              return names.map((name) => ({ schema: "results", name, columns: cols }));
            }),
  };
}

const CM_TYPE: Record<SqlOption["type"], string> = {
  keyword: "keyword",
  function: "function",
  table: "class",
  view: "interface",
  column: "property",
  schema: "namespace",
  catalog: "namespace",
  alias: "variable",
};

function toCompletion(o: SqlOption): Completion {
  const text = o.apply ?? o.label;
  return {
    label: o.label,
    type: CM_TYPE[o.type],
    detail: o.detail,
    apply: o.reopen
      ? (view: EditorView, _c: Completion, from: number, to: number) => {
          view.dispatch({ changes: { from, to, insert: text }, selection: { anchor: from + text.length } });
          setTimeout(() => startCompletion(view), 0);
        }
      : text,
  };
}

// Only the text around the cursor is analysed (a statement rarely spans more).
const WINDOW_BEFORE = 20_000;
const WINDOW_AFTER = 5_000;

/** Completion for the connection returned by `getConnId` (read on each request). */
export function sqlAssist(getConnId: () => string | null | undefined): Extension {
  const source = async (ctx: CompletionContext): Promise<CompletionResult | null> => {
    const id = getConnId();
    const view = ctx.view;
    const doc0 = ctx.state.doc;
    // A late server answer re-opens the list if the user has not typed since.
    const onLate = () => {
      if (view && view.state.doc === doc0) startCompletion(view);
    };
    const p = id ? storeProvider(id, onLate) : null;
    const lineFrom = (pos: number) => ctx.state.doc.lineAt(pos).from;
    const base = ctx.pos > WINDOW_BEFORE ? lineFrom(ctx.pos - WINDOW_BEFORE) : 0;
    const doc = ctx.state.doc.sliceString(base, Math.min(ctx.state.doc.length, ctx.pos + WINDOW_AFTER));
    const provider: MetaProvider = p ?? {
      kind: "postgres",
      schemas: () => undefined,
      objects: async () => undefined,
      cachedObjects: () => [],
      searchTables: async () => [],
      columns: async () => undefined,
    };
    const r = await completeSql(doc, ctx.pos - base, provider, ctx.explicit);
    if (!r || ctx.aborted) return null;
    return { from: base + r.from, options: r.options.map(toCompletion), filter: false };
  };
  return autocompletion({ override: [source], activateOnTyping: true, icons: true, maxRenderedOptions: 80, defaultKeymap: true });
}
