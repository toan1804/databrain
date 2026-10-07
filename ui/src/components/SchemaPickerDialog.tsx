import { useEffect, useMemo, useState } from "react";
import { Database, Library, Search } from "lucide-react";
import { THREE_LEVEL, filterLevel, filterNames, topLevelNoun } from "../lib/catalog";
import { useStore } from "../store";
import { Modal } from "./ui";

/** Rows rendered at once; narrow with the search box beyond that. */
const MAX_ROWS = 400;

/**
 * "Choose catalogs/schemas…": which top-level items the explorer lists for a
 * connection. Catalogs (databases, projects) on Databricks, Snowflake,
 * BigQuery and DuckDB; schemas on the other engines.
 */
export function SchemaPickerDialog() {
  const connId = useStore((s) => s.schemaPicker);
  const conn = useStore((s) => s.connections.find((c) => c.id === s.schemaPicker));
  const schemas = useStore((s) => (s.schemaPicker ? s.schemas[s.schemaPicker] : undefined));
  // Catalog-first engines: pick among catalogs, no schema list needed.
  const catalogs = useStore((s) => (s.schemaPicker && conn && THREE_LEVEL.includes(conn.config.kind) ? s.catalogs[s.schemaPicker] : undefined));
  const catalogSchemas = useStore((s) => s.catalogSchemas);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!connId) return;
    setQuery("");
    setError(null);
    const st = useStore.getState();
    const kind = st.connections.find((c) => c.id === connId)?.config.kind;
    const three = !!kind && THREE_LEVEL.includes(kind);
    const pickFrom = () => {
      const now = useStore.getState();
      const cats = three ? now.catalogs[connId] : undefined;
      // Old filters may hold `catalog.schema` ids: their catalog counts.
      if (cats) return new Set((now.schemaFilter[connId] ?? []).map((c) => (cats.some((k) => k.name === c) ? c : c.split(".")[0])));
      return filterNames(now.schemas[connId] ?? [], now.schemaFilter[connId]);
    };
    setPicked(pickFrom());
    if (three ? !st.catalogs[connId] : !st.schemas[connId]) {
      setLoading(true);
      (three ? st.openExplorer(connId) : st.loadSchemas(connId))
        .then(() => setPicked(pickFrom()))
        .catch((e) => setError(String(e?.message ?? e)))
        .finally(() => setLoading(false));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connId]);

  const level = useMemo(
    () =>
      catalogs
        ? {
            kind: "catalog" as const,
            items: catalogs.map((c) => ({ name: c.name, count: catalogSchemas[`${connId}|${c.name}`]?.length ?? -1, isDefault: c.is_default })),
          }
        : filterLevel(schemas ?? []),
    [catalogs, catalogSchemas, schemas, connId],
  );
  const q = query.trim().toLowerCase();
  const matched = useMemo(() => (q ? level.items.filter((i) => i.name.toLowerCase().includes(q)) : level.items), [level, q]);

  if (!connId || !conn) return null;
  const kind = conn.config.kind;
  const noun = level.kind === "catalog" ? topLevelNoun(kind) : "schema";
  const nouns = `${noun}s`;
  const close = () => useStore.setState({ schemaPicker: null });
  const toggle = (names: string[], on: boolean) =>
    setPicked((p) => {
      const n = new Set(p);
      for (const x of names) if (on) n.add(x);
      else n.delete(x);
      return n;
    });
  const save = () => {
    // Everything (or nothing) picked = no filter, so new ones show up too.
    const list = level.items.filter((i) => picked.has(i.name)).map((i) => i.name);
    useStore.getState().setSchemaFilter(connId, list.length === 0 || list.length === level.items.length ? null : list);
    close();
  };
  const count = level.items.filter((i) => picked.has(i.name)).length;

  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <Database size={15} /> {nouns[0].toUpperCase() + nouns.slice(1)} shown for {conn.name}
        </span>
      }
      onClose={close}
      width={480}
      footer={
        <>
          <span className="mr-auto text-[11.5px] text-muted">
            {count === 0 ? `None selected: all ${nouns} are shown` : `${count} of ${level.items.length} selected`}
          </span>
          <button className="btn-ghost" onClick={close}>
            Cancel
          </button>
          <button className="btn-primary" onClick={save} disabled={!schemas && !catalogs}>
            Save
          </button>
        </>
      }
    >
      <div className="space-y-2">
        <p className="text-[12px] text-muted">
          Only the selected {nouns} are listed in the explorer{level.kind === "catalog" ? `, each with all of its schemas` : ""}. Queries, autocomplete and ⌘P search
          still see everything. {nouns[0].toUpperCase() + nouns.slice(1)} created later are hidden until you add them.
        </p>
        <div className="flex items-center gap-2">
          <div className="relative flex-1">
            <Search size={13} className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-muted" />
            <input
              className="field h-7 pl-7 text-[12.5px]"
              placeholder={`Filter ${level.items.length} ${nouns}…`}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              autoFocus
              aria-label={`Filter ${nouns}`}
            />
          </div>
          <button className="btn-ghost border border-line py-1 text-[12px]" onClick={() => toggle(matched.map((i) => i.name), true)} disabled={!matched.length}>
            Select {q ? "matches" : "all"}
          </button>
          <button className="btn-ghost border border-line py-1 text-[12px]" onClick={() => toggle(matched.map((i) => i.name), false)} disabled={!matched.length}>
            Clear
          </button>
        </div>
        <div className="max-h-[50vh] overflow-auto rounded-md border border-line p-1" role="group" aria-label={nouns}>
          {loading && <div className="p-2 text-[12px] text-muted">Loading {nouns}…</div>}
          {error && <div className="p-2 text-[12px] text-danger">{error}</div>}
          {(schemas || catalogs) && matched.length === 0 && <div className="p-2 text-[12px] text-muted">No {nouns} match.</div>}
          {matched.slice(0, MAX_ROWS).map((i) => (
            <label key={i.name} className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-0.5 hover:bg-hover">
              <input type="checkbox" checked={picked.has(i.name)} onChange={(e) => toggle([i.name], e.target.checked)} />
              {level.kind === "catalog" ? <Library size={12} className="shrink-0 text-muted" /> : <Database size={12} className="shrink-0 text-muted" />}
              <span className="min-w-0 truncate font-mono text-[12px]">{i.name}</span>
              <span className="ml-auto shrink-0 text-[10.5px] text-muted">
                {i.isDefault ? "default" : ""}
                {level.kind === "catalog" && i.count >= 0 && `${i.isDefault ? " · " : ""}${i.count} schema${i.count === 1 ? "" : "s"}`}
              </span>
            </label>
          ))}
          {matched.length > MAX_ROWS && (
            <div className="p-1.5 text-[11.5px] text-muted">
              Showing {MAX_ROWS} of {matched.length}. Type to narrow the list; "Select {q ? "matches" : "all"}" applies to all {matched.length}.
            </div>
          )}
        </div>
      </div>
    </Modal>
  );
}
