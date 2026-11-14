import type { StateCreator } from "zustand";
import { isThreeLevel, mergeCatalogSchemas, prefetchSchemas, revealKeys, splitSchema, treeKey } from "../lib/catalog";
import { api, toError } from "../lib/api";
import type { CatalogInfo, ColumnInfo, DbObject, SchemaInfo, StatementKind } from "../lib/types";
import { chosenSchemaIds } from "./helpers";
import type { ExplorerStatus, State } from "./types";

/**
 * Schema loads in flight (dedupes clicks, prefetch and completion). Keys
 * start with the operation (`list|`, `refresh|`…): calls that answer with
 * different shapes never share one.
 */
const inflight = new Map<string, Promise<unknown>>();
function once<T>(key: string, f: () => Promise<T>): Promise<T> {
  const have = inflight.get(key) as Promise<T> | undefined;
  if (have) return have;
  const p = f().finally(() => inflight.delete(key));
  inflight.set(key, p);
  return p;
}

/** Background prefetch, one schema at a time so it never competes with clicks much. */
let prefetchChain: Promise<unknown> = Promise.resolve();
/** Bumped when a connection's chosen schemas change: older queued loads are dropped. */
const exploreGen = new Map<string, number>();

export interface ExplorerSlice {
  schemas: Record<string, SchemaInfo[]>;
  objects: Record<string, DbObject[]>; // key `${conn}|${schema}`
  columns: Record<string, ColumnInfo[]>; // key `${conn}|${schema}|${name}`
  /** Catalogs of three-level engines, listed before any schema. */
  catalogs: Record<string, CatalogInfo[]>;
  /** Schema lists per catalog (key `${conn}|${catalog}`); merged into `schemas` too. */
  catalogSchemas: Record<string, SchemaInfo[]>;
  /** Where the explorer of a connection comes from (cache / live) and whether it is offline. */
  explorer: Record<string, ExplorerStatus>;
  /** Explorer expansion state (keys from `treeKey`). */
  treeOpen: Record<string, boolean>;
  /** Object key to scroll to and highlight after a search reveal. */
  treeFocus: string | null;
  catalogSearch: { query: string; scope: string | null; focusSeq: number };
  setTreeOpen: (key: string, open: boolean) => void;
  /** Schemas shown in the explorer per connection (absent = all). Saved in settings. */
  schemaFilter: Record<string, string[]>;
  setSchemaFilter: (connId: string, schemas: string[] | null) => void;
  /** Connection whose "Choose schemas" dialog is open. */
  schemaPicker: string | null;
  /** Expand the explorer down to `obj` and scroll to it. */
  revealObject: (connId: string, obj: DbObject) => void;
  /** Show the catalog search (optionally scoped to one connection) and focus it. */
  openCatalogSearch: (scope?: string | null) => void;
  setCatalogSearch: (patch: Partial<{ query: string; scope: string | null }>) => void;
  loadSchemas: (connId: string, force?: boolean) => Promise<SchemaInfo[]>;
  /**
   * Open a connection's explorer: show the local cache at once, then list the
   * top level live (catalogs on three-level engines, schemas elsewhere),
   * check cached schemas for changes and prefetch tables in the background.
   * Throws only when there is neither a cache nor a connection.
   */
  openExplorer: (connId: string, force?: boolean) => Promise<void>;
  /** Schemas of one catalog (cache first, then live). */
  loadCatalogSchemas: (connId: string, catalog: string, force?: boolean) => Promise<SchemaInfo[]>;
  loadObjects: (connId: string, schema: string, force?: boolean) => Promise<DbObject[]>;
  /**
   * Tables of every chosen schema ("Choose schemas/catalogs…"), listed in the
   * background (cache first), so completion knows them without opening each.
   */
  exploreChosen: (connId: string) => Promise<void>;
  loadColumns: (connId: string, schema: string, name: string) => Promise<ColumnInfo[]>;
  /**
   * The server answered (a query finished, a live listing came back): an
   * explorer shown offline from its cache checks it again.
   */
  markOnline: (connId: string) => void;
  /**
   * Which cached schemas changed on the server (one fingerprint query where
   * the engine has one), and those re-listed table by table. `force` skips
   * the "checked a minute ago" shortcut; `withDefault` also checks the
   * default schema (where an unqualified CREATE goes) on engines without
   * fingerprints, which otherwise check only expanded schemas. Never throws.
   */
  checkForChanges: (connId: string, force?: boolean, withDefault?: boolean) => Promise<void>;
  /**
   * DDL ran on the connection (CREATE, ALTER, DROP…): what it changed is
   * shown without a Refresh click. Calls within a moment are merged.
   */
  schemaChanged: (connId: string, schemaList?: boolean) => void;
  /**
   * Forget the connection's cached schemas, tables and columns (asks first)
   * and read them again from the server. `false` when the user said no.
   */
  clearExplorerCache: (connId: string) => Promise<boolean>;
}

/** Schemas of `connId` the tree shows expanded. */
function openSchemas(treeOpen: Record<string, boolean>, connId: string): string[] {
  return Object.entries(treeOpen)
    .filter(([k, v]) => v && k.startsWith(`s|${connId}|`))
    .map(([k]) => k.slice(connId.length + 3));
}

/**
 * Whether a statement that ran may have changed what the explorer shows, and
 * whether it changed the list of schemas itself. Reads never do; DDL, and
 * statements DataBrain can't classify (a procedure may create tables), may.
 */
export function ddlEffect(sql: string, kind: StatementKind): { tables: boolean; schemaList: boolean } {
  if (kind !== "ddl" && kind !== "other" && kind !== "unknown") return { tables: false, schemaList: false };
  const body = sql.replace(/^(\s|--[^\n]*\n|\/\*[\s\S]*?\*\/)*/, "");
  return { tables: true, schemaList: /^(create|drop|alter)\s+(or\s+replace\s+)?(schema|database|catalog)\b/i.test(body) };
}

/**
 * Tables whose columns the explorer keeps in memory. Past this the ones
 * loaded first are dropped (they are read back from the local cache when
 * needed), so a long session over a large catalog doesn't keep growing.
 */
export const MAX_COLUMN_TABLES = 3000;

/** `columns` with `key` set, the oldest entries dropped past the cap. */
export function withColumns<T>(columns: Record<string, T>, key: string, value: T, max = MAX_COLUMN_TABLES): Record<string, T> {
  const { [key]: _, ...rest } = columns;
  const keys = Object.keys(rest);
  const keep = keys.length >= max ? keys.slice(keys.length - max + 1) : keys;
  return { ...Object.fromEntries(keep.map((k) => [k, rest[k]])), [key]: value };
}

/** Pending DDL rechecks per connection (and whether the schema list changed too). */
const ddlTimers = new Map<string, { timer: ReturnType<typeof setTimeout>; schemaList: boolean }>();
/** How long DDL rechecks wait for more statements of the same script. */
export const DDL_RECHECK_MS = 400;

/** `record` without the keys of `connId` (`connId` itself or `connId|…`). */
function withoutConn<T>(record: Record<string, T>, connId: string): Record<string, T> {
  return Object.fromEntries(Object.entries(record).filter(([k]) => k !== connId && !k.startsWith(`${connId}|`)));
}

export const createExplorerSlice: StateCreator<State, [], [], ExplorerSlice> = (set, get) => ({
  clearExplorerCache: (connId) =>
    new Promise<boolean>((resolve) => {
      const conn = get().connections.find((c) => c.id === connId);
      if (!conn) return resolve(false);
      get().askConfirm({
        title: `Clear cached metadata of "${conn.name}"?`,
        reasons: [
          "Forgets the schemas, tables, columns and keys DataBrain keeps for this connection, and reads them again from the server.",
          "Until a schema is read again, the AI can't use its tables (what it may use stays chosen). Notes and AI knowledge choices are kept.",
        ],
        confirmLabel: "Clear and reload",
        onCancel: () => resolve(false),
        onConfirm: async () => {
          try {
            await api.clearExplorerCache(connId);
          } catch (e) {
            get().toast(toError(e).message, "error");
            return resolve(false);
          }
          set((s) => ({
            schemas: withoutConn(s.schemas, connId),
            objects: withoutConn(s.objects, connId),
            columns: withoutConn(s.columns, connId),
            catalogs: withoutConn(s.catalogs, connId),
            catalogSchemas: withoutConn(s.catalogSchemas, connId),
            explorer: withoutConn(s.explorer, connId),
          }));
          resolve(true);
          await get()
            .openExplorer(connId, true)
            .catch((e) => get().toast(`${conn.name}: ${toError(e).message}`, "error"));
        },
      });
    }),
  schemas: {},
  objects: {},
  columns: {},
  catalogs: {},
  catalogSchemas: {},
  explorer: {},
  treeOpen: {},
  schemaFilter: {},
  schemaPicker: null,
  treeFocus: null,
  catalogSearch: { query: "", scope: null, focusSeq: 0 },
  setTreeOpen: (key, open) => set((s) => ({ treeOpen: { ...s.treeOpen, [key]: open } })),
  setSchemaFilter: (connId, schemas) => {
    const next = { ...get().schemaFilter };
    if (schemas && schemas.length) next[connId] = schemas;
    else delete next[connId];
    set({ schemaFilter: next });
    void api.setSetting("explorer_schemas", next).catch(() => {});
    if (schemas && schemas.length) void get().exploreChosen(connId);
    else exploreGen.set(connId, (exploreGen.get(connId) ?? 0) + 1);
  },
  revealObject: (connId, obj) => {
    const st = get();
    const kind = st.connections.find((c) => c.id === connId)?.config.kind;
    const catalog = kind ? splitSchema(kind, obj.schema, st.schemas[connId]).catalog : undefined;
    const open = { ...st.treeOpen };
    for (const k of revealKeys(connId, obj, catalog)) open[k] = true;
    set((s) => ({
      treeOpen: open,
      treeFocus: treeKey.object(connId, obj.schema, obj.name),
      sidebarPanel: "connections",
      catalogSearch: { ...s.catalogSearch, query: "" },
    }));
  },
  openCatalogSearch: (scope) =>
    set((s) => ({
      sidebarPanel: "connections",
      catalogSearch: {
        query: s.catalogSearch.query,
        scope: scope === undefined ? s.catalogSearch.scope : scope,
        focusSeq: s.catalogSearch.focusSeq + 1,
      },
    })),
  setCatalogSearch: (patch) => set((s) => ({ catalogSearch: { ...s.catalogSearch, ...patch } })),

  loadSchemas: async (connId, force = false) => {
    // On three-level engines this may be the explored catalogs' schemas only (enough for completion).
    const cached = get().schemas[connId];
    if (cached && !force) return cached;
    if (!force && !cached) {
      const c = await api.cachedExplorer(connId).catch(() => null);
      if (c?.schemas) {
        set((s) => ({ schemas: { ...s.schemas, [connId]: c.schemas! } }));
        // Up to date in the background.
        void once(`schemas|${connId}`, () => api.listSchemas(connId))
          .then((schemas) => set((s) => ({ schemas: { ...s.schemas, [connId]: schemas } })))
          .catch(() => {});
        return c.schemas;
      }
    }
    const schemas = await once(`schemas|${connId}`, () => api.listSchemas(connId));
    set((s) => ({ schemas: { ...s.schemas, [connId]: schemas } }));
    get()
      .refreshConnections()
      .catch(() => {});
    return schemas;
  },
  openExplorer: async (connId, force = false) => {
    const conn = get().connections.find((c) => c.id === connId);
    if (!conn) return;
    const three = isThreeLevel(conn.config);
    // Refresh: sign in again first, so rights granted since the login (Oracle roles,
    // grants on another schema) show. Tabs mid-statement or mid-transaction are kept.
    if (force) await api.reconnect(connId).catch(() => {});
    // 1. The cache, at once (no network).
    const cache = force ? null : await api.cachedExplorer(connId).catch(() => null);
    const fromCache = !!cache && (three ? !!cache.catalogs : !!cache.schemas);
    if (cache && fromCache) {
      set((s) => {
        const catalogSchemas = { ...s.catalogSchemas };
        let schemas = s.schemas[connId] ?? cache.schemas ?? undefined;
        for (const [cat, list] of Object.entries(cache.catalog_schemas)) {
          catalogSchemas[`${connId}|${cat}`] = list;
          if (!cache.schemas) schemas = mergeCatalogSchemas(schemas, cat, list, cache.catalogs ?? undefined);
        }
        return {
          catalogs: cache.catalogs ? { ...s.catalogs, [connId]: cache.catalogs } : s.catalogs,
          catalogSchemas,
          schemas: schemas ? { ...s.schemas, [connId]: schemas } : s.schemas,
          explorer: { ...s.explorer, [connId]: { ...s.explorer[connId], checkedAt: cache.state.checked_at } },
        };
      });
    }
    // 2. The top level, live.
    const live = (async () => {
      if (three) {
        const cats = await once(`catalogs|${connId}`, () => api.listCatalogs(connId));
        if (cats) {
          set((s) => ({ catalogs: { ...s.catalogs, [connId]: cats } }));
          return;
        }
      }
      const schemas = await once(`schemas|${connId}`, () => api.listSchemas(connId));
      set((s) => ({ schemas: { ...s.schemas, [connId]: schemas } }));
    })();
    try {
      await live;
    } catch (e) {
      if (!fromCache) throw e;
      // Offline: keep the cached tree, say so.
      set((s) => ({ explorer: { ...s.explorer, [connId]: { ...s.explorer[connId], offline: toError(e).message, checking: false } } }));
      return;
    }
    set((s) => ({ explorer: { ...s.explorer, [connId]: { ...s.explorer[connId], offline: null } } }));
    get()
      .refreshConnections()
      .catch(() => {});
    // 3. Background: which cached schemas changed (refreshed table by table), then prefetch.
    void (async () => {
      const open = openSchemas(get().treeOpen, connId);
      await get().checkForChanges(connId, force);
      // Default catalog's schemas, then tables of likely-clicked schemas.
      const cats = get().catalogs[connId];
      if (three && cats) {
        const want = cats.filter((c) => c.is_default || get().treeOpen[treeKey.catalog(connId, c.name)]).slice(0, 4);
        for (const c of want) await get().loadCatalogSchemas(connId, c.name).catch(() => {});
      }
      const all = get().schemas[connId] ?? [];
      const have = new Set<string>();
      for (const sc of all) {
        if (get().objects[`${connId}|${sc.name}`]) have.add(sc.name);
      }
      const cachedListed = await Promise.all(
        prefetchSchemas(all, new Set(open), have, 16).map(async (name) => [name, await api.cachedObjects(connId, name).catch(() => null)] as const),
      );
      const todo = cachedListed.filter(([, c]) => !c).map(([n]) => n).slice(0, 8);
      for (const name of todo) {
        prefetchChain = prefetchChain.then(() => (get().explorer[connId]?.offline ? undefined : get().loadObjects(connId, name).catch(() => {})));
      }
      // Chosen schemas: all of them, so completion knows their tables.
      void get().exploreChosen(connId);
    })();
  },
  exploreChosen: async (connId) => {
    const chosen = get().schemaFilter[connId];
    if (!chosen?.length) return;
    const gen = (exploreGen.get(connId) ?? 0) + 1;
    exploreGen.set(connId, gen);
    const current = () => exploreGen.get(connId) === gen && !get().explorer[connId]?.offline;
    const cfg = get().connections.find((c) => c.id === connId)?.config;
    const cats = cfg && isThreeLevel(cfg) ? get().catalogs[connId] : undefined;
    let ids: string[];
    if (cats) {
      // Catalog filter (older ones held `catalog.schema` ids: their catalog counts).
      const names = [...new Set(chosen.map((c) => (cats.some((k) => k.name === c) ? c : c.split(".")[0])))].filter((c) => cats.some((k) => k.name === c));
      ids = [];
      for (const cat of names) {
        if (!current()) return;
        const list = await get().loadCatalogSchemas(connId, cat).catch(() => [] as SchemaInfo[]);
        ids.push(...list.map((s) => s.name));
      }
    } else {
      const all = get().schemas[connId] ?? (await get().loadSchemas(connId).catch(() => [] as SchemaInfo[]));
      ids = chosenSchemaIds(all, chosen);
    }
    for (const id of ids) {
      if (get().objects[`${connId}|${id}`]) continue;
      prefetchChain = prefetchChain.then(() => (current() && !get().objects[`${connId}|${id}`] ? get().loadObjects(connId, id).catch(() => {}) : undefined));
    }
    await prefetchChain;
  },
  loadCatalogSchemas: async (connId, catalog, force = false) => {
    const key = `${connId}|${catalog}`;
    const put = (list: SchemaInfo[]) =>
      set((s) => ({
        catalogSchemas: { ...s.catalogSchemas, [key]: list },
        schemas: { ...s.schemas, [connId]: mergeCatalogSchemas(s.schemas[connId], catalog, list, s.catalogs[connId]) },
      }));
    const have = get().catalogSchemas[key];
    if (have && !force) {
      // From the cache: bring it up to date once per session, quietly.
      if (!get().explorer[connId]?.offline)
        void once(`cat|${key}`, () => api.listCatalogSchemas(connId, catalog))
          .then((list) => JSON.stringify(list) !== JSON.stringify(get().catalogSchemas[key]) && put(list))
          .catch(() => {});
      return have;
    }
    const list = await once(`cat|${key}`, () => api.listCatalogSchemas(connId, catalog));
    put(list);
    return list;
  },
  loadObjects: async (connId, schema, force = false) => {
    const key = `${connId}|${schema}`;
    const have = get().objects[key];
    const put = (objs: DbObject[]) => set((s) => ({ objects: { ...s.objects, [key]: objs } }));
    if (have && !force) {
      // In memory, but its check was put off (no fingerprints, or past the
      // per-check cap): check it now, once.
      const ex = get().explorer[connId];
      if (ex && !ex.offline && ex.deferred?.includes(schema)) {
        set((s) => ({ explorer: { ...s.explorer, [connId]: { ...s.explorer[connId], deferred: s.explorer[connId]?.deferred?.filter((x) => x !== schema) } } }));
        void once(`refresh|${key}`, () => api.refreshSchema(connId, schema))
          .then((r) => r.mode !== "unchanged" && put(r.objects))
          .catch(() => {});
      }
      return have;
    }
    if (force) {
      // Refresh button: only changed tables are fetched when the engine has versions.
      const r = await once(`refresh|${key}`, () => api.refreshSchema(connId, schema));
      get().markOnline(connId);
      put(r.objects);
      if (r.mode !== "unchanged") {
        const pre = `${key}|`;
        set((s) => ({ columns: Object.fromEntries(Object.entries(s.columns).filter(([k]) => !k.startsWith(pre))) }));
      }
      return r.objects;
    }
    const cached = await api.cachedObjects(connId, schema).catch(() => null);
    if (cached) {
      put(cached.objects);
      const ex = get().explorer[connId];
      // Not covered by the fingerprint check (none on this engine, or deferred): check it now.
      if (ex && !ex.offline && (ex.fingerprints === false || ex.deferred?.includes(schema)))
        void once(`refresh|${key}`, () => api.refreshSchema(connId, schema))
          .then((r) => r.mode !== "unchanged" && put(r.objects))
          .catch(() => {});
      return cached.objects;
    }
    const objs = await once(`list|${key}`, () => api.listObjects(connId, schema));
    get().markOnline(connId);
    put(objs);
    return objs;
  },
  loadColumns: async (connId, schema, name) => {
    const key = `${connId}|${schema}|${name}`;
    const have = get().columns[key];
    if (have) return have;
    const put = (cols: ColumnInfo[]) => set((s) => ({ columns: withColumns(s.columns, key, cols) }));
    const live = () =>
      once(`col|${key}`, () => api.describeObject(connId, schema, name)).then((d) => (get().markOnline(connId), put(d.columns), d.columns));
    const cached = await api.cachedColumns(connId, schema, name).catch(() => null);
    if (cached) {
      put(cached.columns);
      // Changed since: shown now, replaced when the server answers.
      if (cached.stale && !get().explorer[connId]?.offline) void live().catch(() => {});
      return cached.columns;
    }
    return live();
  },
  checkForChanges: async (connId, force = false, withDefault = false) => {
    const st = get();
    if (st.explorer[connId]?.offline) return;
    const open = openSchemas(st.treeOpen, connId);
    const known = st.schemas[connId] ?? Object.entries(st.catalogSchemas).flatMap(([k, v]) => (k.startsWith(`${connId}|`) ? v : []));
    const def = withDefault ? known.find((x) => x.is_default)?.name : undefined;
    if (def && !open.includes(def)) open.push(def);
    set((s) => ({ explorer: { ...s.explorer, [connId]: { ...s.explorer[connId], checking: true } } }));
    try {
      const r = await api.revalidateExplorer(connId, open, force);
      set((s) => {
        const objects = { ...s.objects };
        let columns = s.columns;
        for (const x of r.refreshed) {
          if (x.mode === "unchanged") continue;
          objects[`${connId}|${x.schema}`] = x.objects;
          // Columns re-read from the cache (changed tables are marked stale there).
          const pre = `${connId}|${x.schema}|`;
          columns = Object.fromEntries(Object.entries(columns).filter(([k]) => !k.startsWith(pre)));
        }
        const prev = s.explorer[connId];
        return {
          objects,
          columns,
          explorer: {
            ...s.explorer,
            [connId]: {
              ...prev,
              checking: false,
              checkedAt: r.skipped ? prev?.checkedAt : Date.now(),
              fingerprints: r.skipped ? prev?.fingerprints : r.fingerprints,
              deferred: r.skipped ? prev?.deferred : r.deferred,
            },
          },
        };
      });
    } catch {
      set((s) => ({ explorer: { ...s.explorer, [connId]: { ...s.explorer[connId], checking: false } } }));
    }
  },
  schemaChanged: (connId, schemaList = false) => {
    const was = ddlTimers.get(connId);
    if (was) clearTimeout(was.timer);
    const list = schemaList || !!was?.schemaList;
    const timer = setTimeout(() => {
      ddlTimers.delete(connId);
      void (async () => {
        if (list && get().schemas[connId]) await get().loadSchemas(connId, true).catch(() => {});
        await get().checkForChanges(connId, true, true);
      })();
    }, DDL_RECHECK_MS);
    ddlTimers.set(connId, { timer, schemaList: list });
  },
  markOnline: (connId) => {
    if (!get().explorer[connId]?.offline) return;
    set((s) => ({ explorer: { ...s.explorer, [connId]: { ...s.explorer[connId], offline: null } } }));
    // What changed while it was away: checked as on connect.
    void get()
      .openExplorer(connId)
      .catch(() => {});
  },
});
