import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { AlertCircle, ChevronDown, Eye, Loader2, Play, Search, Table2, TextCursorInput, X } from "lucide-react";
import { api, toError } from "../lib/api";
import { RESULTS_SCHEMA_ID, cachedHits, matchRange, objectMatches, rankHits, splitSchema, tablePath } from "../lib/catalog";
import type { ConnectionView, DbObject } from "../lib/types";
import { selectTopSql } from "../lib/util";
import { editorBridge } from "../editorBridge";
import { useStore } from "../store";
import { ConnDot, MenuItem, Popover } from "./ui";
import { ObjectMenu } from "./CatalogMenus";

/** SQL reference for an object (DuckDB attached files are addressed as `files.x`). */
export function objectRef(conn: ConnectionView, obj: DbObject): string {
  return tablePath(conn.config.kind, obj.schema, obj.name);
}

/** Open a new tab with `SELECT * … LIMIT 100` for the object and run it. */
export function selectTop(conn: ConnectionView, obj: DbObject) {
  const sql =
    conn.config.kind === "duckdb" && (obj.schema.endsWith(".files") || obj.schema === RESULTS_SCHEMA_ID)
      ? `SELECT *\nFROM ${objectRef(conn, obj)}\nLIMIT 100;`
      : selectTopSql(conn.config.kind, obj.schema, obj.name);
  const st = useStore.getState();
  const id = st.newTab({ title: obj.name, sql, connection_id: conn.id });
  void st.runTab(id, "all", { doc: sql, selFrom: 0, selTo: 0, cursor: 0 });
}

interface Remote {
  hits: DbObject[];
  loading: boolean;
  error?: string;
}

const LIMIT = 100;

/**
 * Explorer search box. With an empty query it renders `tree`; otherwise it
 * lists matching tables/views from every connected database (or the scoped
 * connection) and the connections whose name matches (`connectionsTree`).
 */
export function CatalogSearch({ tree, connectionsTree }: { tree: ReactNode; connectionsTree: ReactNode | null }) {
  const connections = useStore((s) => s.connections);
  const objects = useStore((s) => s.objects);
  const schemas = useStore((s) => s.schemas);
  const { query, scope, focusSeq } = useStore((s) => s.catalogSearch);
  const setSearch = useStore((s) => s.setCatalogSearch);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const [remote, setRemote] = useState<Record<string, Remote>>({});
  const [active, setActive] = useState(0);
  const [scopeMenu, setScopeMenu] = useState<{ x: number; y: number } | null>(null);
  const seq = useRef(0);

  const q = query.trim();
  const scoped = scope ? connections.find((c) => c.id === scope) : undefined;
  const targets = useMemo(
    () => (scoped ? [scoped] : connections.filter((c) => c.connected)),
    [connections, scoped],
  );
  const targetKey = targets.map((c) => c.id).join(",");

  useEffect(() => {
    if (focusSeq === 0) return;
    inputRef.current?.focus();
    inputRef.current?.select();
  }, [focusSeq]);

  // Local metadata cache (tables seen before, any session): instant, no debounce.
  const [local, setLocal] = useState<Record<string, DbObject[]>>({});
  const localSeq = useRef(0);
  useEffect(() => {
    if (!q) {
      setLocal({});
      return;
    }
    const mine = ++localSeq.current;
    const term = q.trim().split(".").pop() ?? "";
    for (const c of targets) {
      api
        .completeTablesLocal(c.id, null, term, LIMIT * 2)
        .then((hits) => {
          if (localSeq.current === mine) setLocal((l) => ({ ...l, [c.id]: hits }));
        })
        .catch(() => {});
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [q, targetKey]);

  // Server-side search, debounced; stale responses are dropped.
  useEffect(() => {
    if (!q) {
      setRemote({});
      return;
    }
    const mine = ++seq.current;
    setRemote((r) => Object.fromEntries(targets.map((c) => [c.id, { hits: r[c.id]?.hits ?? [], loading: true }])));
    const t = setTimeout(() => {
      for (const c of targets) {
        api
          .searchObjects(c.id, q, LIMIT)
          .then((hits) => {
            if (seq.current === mine) setRemote((r) => ({ ...r, [c.id]: { hits, loading: false } }));
          })
          .catch((e) => {
            if (seq.current === mine) setRemote((r) => ({ ...r, [c.id]: { hits: [], loading: false, error: toError(e).message } }));
          })
          .finally(() => {
            // A scoped search may have just connected.
            if (scoped && !scoped.connected) void useStore.getState().refreshConnections();
          });
      }
    }, 200);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [q, targetKey]);

  // Cached explorer objects show instantly; server hits are merged in.
  const sections = useMemo(() => {
    if (!q) return [];
    return targets.map((c) => {
      const r = remote[c.id];
      const merged = [
        ...cachedHits(q, c.id, objects),
        ...(local[c.id] ?? []).filter((o) => objectMatches(q, o.schema, o.name)),
        ...(r?.hits ?? []).filter((o) => objectMatches(q, o.schema, o.name)),
      ];
      return { conn: c, hits: rankHits(q, merged, LIMIT), loading: r?.loading ?? true, error: r?.error };
    });
  }, [q, targets, remote, local, objects]);
  const flat = useMemo(() => sections.flatMap((s) => s.hits.map((obj) => ({ conn: s.conn, obj }))), [sections]);

  useEffect(() => setActive(0), [q, targetKey]);
  useEffect(() => {
    listRef.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
  }, [active]);

  const reveal = (conn: ConnectionView, obj: DbObject) => useStore.getState().revealObject(conn.id, obj);
  const insert = (conn: ConnectionView, obj: DbObject) => {
    const tab = useStore.getState().activeTabId;
    if (tab) editorBridge.insert(tab, objectRef(conn, obj));
  };

  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      if (query) setSearch({ query: "" });
      else inputRef.current?.blur();
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!flat.length) return;
      const d = e.key === "ArrowDown" ? 1 : -1;
      setActive((i) => (i + d + flat.length) % flat.length);
    } else if (e.key === "Enter" && flat[active]) {
      e.preventDefault();
      const { conn, obj } = flat[active];
      if (e.metaKey || e.ctrlKey) selectTop(conn, obj);
      else if (e.altKey) insert(conn, obj);
      else reveal(conn, obj);
    }
  };

  let idx = -1;
  return (
    <>
      <div className="px-2 pb-2">
        <div className="relative flex items-center">
          <Search size={13} className="pointer-events-none absolute left-2 text-muted" />
          <input
            ref={inputRef}
            title="Find a table or connection (⌘P)"
            className="field py-1 pl-7 pr-[7.5rem]"
            placeholder="Find table…"
            value={query}
            aria-label="Find table or connection (⌘P)"
            aria-controls="catalog-search-results"
            spellCheck={false}
            autoCorrect="off"
            onChange={(e) => setSearch({ query: e.target.value })}
            onKeyDown={onKey}
          />
          <div className="absolute right-1 flex items-center gap-0.5">
            <button
              className="flex h-5 max-w-[6.5rem] items-center gap-1 rounded px-1.5 text-[11px] text-muted hover:bg-hover hover:text-fg"
              title="Where to search"
              aria-label={`Search scope: ${scoped ? scoped.name : "all connected"}`}
              onClick={(e) => {
                const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
                setScopeMenu({ x: r.left, y: r.bottom + 4 });
              }}
            >
              {scoped && <ConnDot color={scoped.color} connected={scoped.connected} />}
              <span className="truncate">{scoped ? scoped.name : "Connected"}</span>
              <ChevronDown size={11} className="shrink-0" />
            </button>
            {(query || scoped) && (
              <button
                className="icon-btn h-5 w-5"
                title="Clear search"
                aria-label="Clear search"
                onClick={() => {
                  setSearch({ query: "", scope: null });
                  inputRef.current?.focus();
                }}
              >
                <X size={12} />
              </button>
            )}
          </div>
        </div>
      </div>
      {scopeMenu && (
        <Popover x={scopeMenu.x} y={scopeMenu.y} onClose={() => setScopeMenu(null)} className="max-h-80 w-60 overflow-auto">
          <MenuItem
            icon={<Search size={13} />}
            label="All connected databases"
            hint={!scope ? "✓" : undefined}
            onClick={() => {
              setScopeMenu(null);
              setSearch({ scope: null });
              inputRef.current?.focus();
            }}
          />
          {connections.map((c) => (
            <MenuItem
              key={c.id}
              icon={<ConnDot color={c.color} connected={c.connected} />}
              label={c.name}
              hint={scope === c.id ? "✓" : undefined}
              onClick={() => {
                setScopeMenu(null);
                setSearch({ scope: c.id });
                inputRef.current?.focus();
              }}
            />
          ))}
        </Popover>
      )}

      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-3">
        {!q ? (
          tree
        ) : (
          <>
            <div id="catalog-search-results" ref={listRef} role="listbox" aria-label="Matching tables">
              {targets.length === 0 && (
                <div className="px-3 py-4 text-center text-[12px] text-muted">
                  No connected databases. Connect one, or pick a connection from the scope menu to search it.
                </div>
              )}
              {sections.map((s) => (
                <div key={s.conn.id} className="mb-1">
                  <div className="flex h-[24px] items-center gap-1.5 px-1.5 text-[11.5px] text-muted">
                    <ConnDot color={s.conn.color} connected={s.conn.connected} />
                    <span className="min-w-0 truncate font-medium">{s.conn.name}</span>
                    <span className="flex-1" />
                    {s.loading ? <Loader2 size={11} className="animate-spin" /> : <span>{s.hits.length >= LIMIT ? `${LIMIT}+` : s.hits.length}</span>}
                  </div>
                  {s.error && (
                    <div className="flex items-start gap-1.5 px-2 py-1 text-[11.5px] text-danger" title={s.error}>
                      <AlertCircle size={12} className="mt-0.5 shrink-0" />
                      <span className="line-clamp-2">{s.error}</span>
                    </div>
                  )}
                  {!s.loading && !s.error && s.hits.length === 0 && (
                    <div className="px-6 py-1 text-[12px] text-muted">No tables match</div>
                  )}
                  {s.hits.map((obj) => {
                    const i = ++idx;
                    return (
                      <HitRow
                        key={`${obj.schema}\u0000${obj.name}`}
                        conn={s.conn}
                        obj={obj}
                        query={q}
                        path={splitSchema(s.conn.config.kind, obj.schema, schemas[s.conn.id])}
                        selected={i === active}
                        onHover={() => setActive(i)}
                        onReveal={() => reveal(s.conn, obj)}
                        onInsert={() => insert(s.conn, obj)}
                      />
                    );
                  })}
                </div>
              ))}
            </div>
            {flat.length > 0 && (
              <div className="px-2 pt-1 text-[10.5px] leading-4 text-muted">
                ↵ show in tree · ⌘↵ select top 100 · ⌥↵ insert name
              </div>
            )}
            {connectionsTree && (
              <>
                <div className="mt-3 px-1.5 pb-1 text-[11px] font-semibold uppercase tracking-wider text-muted">Connections</div>
                {connectionsTree}
              </>
            )}
          </>
        )}
      </div>
    </>
  );
}

function HitRow({
  conn,
  obj,
  query,
  path,
  selected,
  onHover,
  onReveal,
  onInsert,
}: {
  conn: ConnectionView;
  obj: DbObject;
  query: string;
  path: { catalog?: string; schema: string };
  selected: boolean;
  onHover: () => void;
  onReveal: () => void;
  onInsert: () => void;
}) {
  const m = matchRange(query, obj.name);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const isView = obj.kind === "view" || obj.kind === "materialized_view";
  const where = path.catalog ? `${path.catalog} › ${path.schema}` : path.schema;
  return (
    <div
      role="option"
      aria-selected={selected}
      title={`${where} › ${obj.name}${obj.comment ? `\n${obj.comment}` : ""}`}
      onMouseMove={onHover}
      onClick={onReveal}
      onDoubleClick={onInsert}
      onContextMenu={(e) => {
        e.preventDefault();
        setMenu({ x: e.clientX, y: e.clientY });
      }}
      className={`group flex h-[26px] cursor-pointer items-center gap-1.5 rounded-md pl-5 pr-1 text-[13px] ${selected ? "bg-hover" : "hover:bg-hover"}`}
    >
      <span className="flex shrink-0 text-muted">{isView ? <Eye size={13} /> : <Table2 size={13} />}</span>
      <span className="min-w-0 shrink truncate">
        {m ? (
          <>
            {obj.name.slice(0, m[0])}
            <mark className="rounded-sm bg-accent/25 text-fg">{obj.name.slice(m[0], m[1])}</mark>
            {obj.name.slice(m[1])}
          </>
        ) : (
          obj.name
        )}
      </span>
      <span className={`ml-auto min-w-0 shrink-[2] truncate pl-2 text-right text-[11px] text-muted ${selected ? "hidden" : "group-hover:hidden"}`}>
        {where}
      </span>
      {menu && (
        // Portal events bubble through React parents: keep menu clicks off the row.
        <span onClick={(e) => e.stopPropagation()} onDoubleClick={(e) => e.stopPropagation()} onMouseMove={(e) => e.stopPropagation()}>
          <ObjectMenu conn={conn} obj={obj} at={menu} onClose={() => setMenu(null)} onSelectTop={() => selectTop(conn, obj)} />
        </span>
      )}
      <span className={`shrink-0 items-center ${selected ? "flex" : "hidden group-hover:flex"}`}>
        <button
          className="icon-btn h-6 w-6"
          title="Insert name into the editor (⌥↵)"
          aria-label="Insert name into the editor"
          onClick={(e) => {
            e.stopPropagation();
            onInsert();
          }}
        >
          <TextCursorInput size={12} />
        </button>
        <button
          className="icon-btn h-6 w-6"
          title="Select top 100 rows (⌘↵)"
          aria-label="Select top 100 rows"
          onClick={(e) => {
            e.stopPropagation();
            selectTop(conn, obj);
          }}
        >
          <Play size={12} />
        </button>
      </span>
    </div>
  );
}
