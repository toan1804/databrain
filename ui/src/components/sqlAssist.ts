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

// Server searches are cached briefly per connection + prefix.
const searchCache = new Map<string, { at: number; p: Promise<DbObject[]> }>();
const loading = new Set<string>();

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

/** Metadata for completion, read live from the store (loads on demand). */
export function storeProvider(connId: string): MetaProvider | null {
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
  return {
    kind: conn.config.kind,
    schemas: () => {
      const s = useStore.getState().schemas[connId];
      if (!live) return s;
      if (!s) once(`s|${connId}`, () => useStore.getState().loadSchemas(connId));
      else {
        // Default schema's tables are suggested without typing a schema.
        const def = s.find((x) => x.is_default) ?? (s.length === 1 ? s[0] : undefined);
        if (def && !useStore.getState().objects[`${connId}|${def.name}`]) once(`o|${connId}|${def.name}`, () => useStore.getState().loadObjects(connId, def.name));
      }
      return s;
    },
    objects: async (schema) => {
      if (!live) return useStore.getState().objects[`${connId}|${schema}`];
      try {
        return await useStore.getState().loadObjects(connId, schema);
      } catch {
        return undefined;
      }
    },
    cachedObjects: () => cachedIndex(connId).all,
    cachedNamed: (name) => cachedIndex(connId).byName.get(name.toLowerCase()) ?? [],
    searchTables: (prefix) => {
      if (!live) return Promise.resolve([]);
      const key = `${connId}|${prefix.toLowerCase()}`;
      const hit = searchCache.get(key);
      if (hit && Date.now() - hit.at < 60_000) return hit.p;
      const p = api.searchObjects(connId, prefix, 50).catch(() => [] as DbObject[]);
      searchCache.set(key, { at: Date.now(), p });
      if (searchCache.size > 300) searchCache.delete(searchCache.keys().next().value!);
      return p;
    },
    columns: async (schema, table) => {
      if (!live) return useStore.getState().columns[`${connId}|${schema}|${table}`]?.map((c) => c.name);
      try {
        return (await useStore.getState().loadColumns(connId, schema, table)).map((c) => c.name);
      } catch {
        return undefined;
      }
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

/** Completion for the connection returned by `getConnId` (read on each request). */
export function sqlAssist(getConnId: () => string | null | undefined): Extension {
  const source = async (ctx: CompletionContext): Promise<CompletionResult | null> => {
    const id = getConnId();
    const p = id ? storeProvider(id) : null;
    const doc = ctx.state.doc.toString();
    const provider: MetaProvider = p ?? {
      kind: "postgres",
      schemas: () => undefined,
      objects: async () => undefined,
      cachedObjects: () => [],
      searchTables: async () => [],
      columns: async () => undefined,
    };
    const r = await completeSql(doc, ctx.pos, provider, ctx.explicit);
    if (!r || ctx.aborted) return null;
    return { from: r.from, options: r.options.map(toCompletion), filter: false };
  };
  return autocompletion({ override: [source], activateOnTyping: true, icons: true, maxRenderedOptions: 80, defaultKeymap: true });
}
