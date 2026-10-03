import { useEffect, useMemo, useState } from "react";
import { Database, Search } from "lucide-react";
import { groupSchemas, schemaLabel } from "../lib/catalog";
import type { SchemaInfo } from "../lib/types";
import { useStore } from "../store";
import { Modal } from "./ui";

/** Rows rendered at once; narrow with the search box beyond that. */
const MAX_ROWS = 400;

/** "Choose schemas…" for a connection: which schemas the explorer lists. */
export function SchemaPickerDialog() {
  const connId = useStore((s) => s.schemaPicker);
  const conn = useStore((s) => s.connections.find((c) => c.id === s.schemaPicker));
  const schemas = useStore((s) => (s.schemaPicker ? s.schemas[s.schemaPicker] : undefined));
  const saved = useStore((s) => (s.schemaPicker ? s.schemaFilter[s.schemaPicker] : undefined));
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!connId) return;
    setQuery("");
    setError(null);
    setPicked(new Set(saved ?? []));
    if (!useStore.getState().schemas[connId]) {
      setLoading(true);
      useStore
        .getState()
        .loadSchemas(connId)
        .catch((e) => setError(String(e?.message ?? e)))
        .finally(() => setLoading(false));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connId]);

  const all = useMemo(() => schemas ?? [], [schemas]);
  const q = query.trim().toLowerCase();
  const matched = useMemo(() => (q ? all.filter((s) => s.name.toLowerCase().includes(q)) : all), [all, q]);
  const groups = useMemo(() => groupSchemas(matched.slice(0, MAX_ROWS)), [matched]);

  if (!connId || !conn) return null;
  const close = () => useStore.setState({ schemaPicker: null });
  const toggle = (names: string[], on: boolean) =>
    setPicked((p) => {
      const n = new Set(p);
      for (const x of names) if (on) n.add(x);
      else n.delete(x);
      return n;
    });
  const save = () => {
    // Everything picked = no filter (new schemas then show up too).
    const list = all.filter((s) => picked.has(s.name)).map((s) => s.name);
    useStore.getState().setSchemaFilter(connId, list.length === 0 || list.length === all.length ? null : list);
    close();
  };
  const row = (s: SchemaInfo, indent: boolean) => (
    <label key={s.name} className={`flex cursor-pointer items-center gap-2 rounded px-1.5 py-0.5 hover:bg-hover ${indent ? "pl-6" : ""}`}>
      <input type="checkbox" checked={picked.has(s.name)} onChange={(e) => toggle([s.name], e.target.checked)} />
      <span className="min-w-0 truncate font-mono text-[12px]">{indent ? schemaLabel(s) : s.name}</span>
      {s.is_default && <span className="ml-auto shrink-0 text-[10.5px] text-muted">default</span>}
    </label>
  );

  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <Database size={15} /> Schemas shown for {conn.name}
        </span>
      }
      onClose={close}
      width={520}
      footer={
        <>
          <span className="mr-auto text-[11.5px] text-muted">
            {picked.size === 0 ? "None selected: all schemas are shown" : `${picked.size} of ${all.length} selected`}
          </span>
          <button className="btn-ghost" onClick={close}>
            Cancel
          </button>
          <button className="btn-primary" onClick={save} disabled={!schemas}>
            Save
          </button>
        </>
      }
    >
      <div className="space-y-2">
        <p className="text-[12px] text-muted">
          Only the selected schemas are listed in the explorer. Queries, autocomplete and ⌘P search still see every schema. Schemas created later are hidden until you add them.
        </p>
        <div className="flex items-center gap-2">
          <div className="relative flex-1">
            <Search size={13} className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-muted" />
            <input className="field h-7 pl-7 text-[12.5px]" placeholder={`Filter ${all.length} schemas…`} value={query} onChange={(e) => setQuery(e.target.value)} autoFocus aria-label="Filter schemas" />
          </div>
          <button className="btn-ghost border border-line py-1 text-[12px]" onClick={() => toggle(matched.map((s) => s.name), true)} disabled={!matched.length}>
            Select {q ? "matches" : "all"}
          </button>
          <button className="btn-ghost border border-line py-1 text-[12px]" onClick={() => toggle(matched.map((s) => s.name), false)} disabled={!matched.length}>
            Clear
          </button>
        </div>
        <div className="max-h-[50vh] overflow-auto rounded-md border border-line p-1" role="group" aria-label="Schemas">
          {loading && <div className="p-2 text-[12px] text-muted">Loading schemas…</div>}
          {error && <div className="p-2 text-[12px] text-danger">{error}</div>}
          {schemas && matched.length === 0 && <div className="p-2 text-[12px] text-muted">No schemas match.</div>}
          {groups
            ? groups.map((g) => {
                const names = g.schemas.map((s) => s.name);
                const on = names.filter((n) => picked.has(n)).length;
                return (
                  <div key={g.name}>
                    <label className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-0.5 font-medium hover:bg-hover">
                      <input
                        type="checkbox"
                        checked={on === names.length}
                        ref={(el) => {
                          if (el) el.indeterminate = on > 0 && on < names.length;
                        }}
                        onChange={(e) => toggle(names, e.target.checked)}
                      />
                      <span className="min-w-0 truncate text-[12.5px]">{g.name}</span>
                      <span className="ml-auto text-[10.5px] text-muted">
                        {on}/{names.length}
                      </span>
                    </label>
                    {g.schemas.map((s) => row(s, true))}
                  </div>
                );
              })
            : matched.slice(0, MAX_ROWS).map((s) => row(s, false))}
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
