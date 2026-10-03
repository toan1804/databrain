import { useEffect, useMemo, useRef, useState } from "react";
import { Check, Columns3, Loader2, Table2, TriangleAlert } from "lucide-react";
import { api } from "../lib/api";
import { applySuggestion, currentSegment, findTable, suggest, type Suggestion, type TargetTable } from "../lib/noteTarget";
import type { DbObject, KnObject } from "../lib/types";
import { useStore } from "../store";
import { canFetchMetadata } from "./sqlAssist";

export type TargetState = { status: "empty" | "checking" | "ok" | "invalid"; canonical?: string; error?: string };

/**
 * Target field of a note: tables of the connection with completion
 * (`schema.table`, `schema.table.column`, several joined with `&`, `and`,
 * `or`). Each change is checked against the connection.
 */
export function NoteTargetInput({
  connId,
  value,
  onChange,
  indexed,
  onState,
}: {
  connId: string;
  value: string;
  onChange: (v: string) => void;
  /** Knowledge index objects (with columns) of the connection. */
  indexed: KnObject[];
  onState: (s: TargetState) => void;
}) {
  const ref = useRef<HTMLInputElement>(null);
  const [cursor, setCursor] = useState(0);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [found, setFound] = useState<DbObject[]>([]);
  const [state, setState] = useState<TargetState>({ status: "empty" });
  const cached = useStore((s) => s.objects);
  const columnsCache = useStore((s) => s.columns);
  const conn = useStore((s) => s.connections.find((c) => c.id === connId));
  const live = !!conn && canFetchMetadata(conn);

  const seg = currentSegment(value, cursor);
  const tables = useMemo<TargetTable[]>(() => {
    const out: TargetTable[] = indexed.map((o) => ({ schema: o.schema, name: o.name, columns: o.columns.map((c) => c.name) }));
    const add = (o: DbObject) => {
      if (o.kind === "function" || o.kind === "procedure" || o.kind === "sequence" || o.kind === "other") return;
      out.push({ schema: o.schema, name: o.name, columns: columnsCache[`${connId}|${o.schema}|${o.name}`]?.map((c) => c.name) });
    };
    for (const [k, list] of Object.entries(cached)) if (k.startsWith(`${connId}|`)) list.forEach(add);
    found.forEach(add);
    return out;
  }, [indexed, cached, columnsCache, found, connId]);
  const items: Suggestion[] = useMemo(() => (open ? suggest(seg.word, tables) : []), [open, seg.word, tables]);

  // Server search for tables not loaded yet.
  useEffect(() => {
    const w = seg.word.split(".").pop() ?? "";
    if (!open || !live || w.length < 2) return;
    const t = setTimeout(() => void api.searchObjects(connId, w, 30).then(setFound).catch(() => {}), 250);
    return () => clearTimeout(t);
  }, [open, live, connId, seg.word]);

  // Columns after `table.` when not known yet.
  useEffect(() => {
    const dot = seg.word.lastIndexOf(".");
    if (!open || !live || dot <= 0) return;
    const t = findTable(tables, seg.word.slice(0, dot));
    if (t && !t.columns) void useStore.getState().loadColumns(connId, t.schema, t.name).catch(() => {});
  }, [open, live, connId, seg.word, tables]);

  // Check the whole target (debounced).
  useEffect(() => {
    if (!value.trim()) {
      setState({ status: "empty" });
      return;
    }
    setState({ status: "checking" });
    let stale = false;
    const t = setTimeout(() => {
      api
        .knCheckTarget(connId, value)
        .then((r) => !stale && setState(r.ok ? { status: "ok", canonical: r.target ?? undefined } : { status: "invalid", error: r.error ?? "not found" }))
        .catch((e) => !stale && setState({ status: "invalid", error: String(e?.message ?? e) }));
    }, 350);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [connId, value]);
  useEffect(() => onState(state), [state]); // eslint-disable-line react-hooks/exhaustive-deps

  const accept = (s: Suggestion) => {
    const r = applySuggestion(value, seg, s.insert);
    onChange(r.text);
    setOpen(s.kind === "table");
    setActive(0);
    requestAnimationFrame(() => {
      ref.current?.setSelectionRange(r.cursor, r.cursor);
      setCursor(r.cursor);
    });
  };

  return (
    <div className="relative">
      <div className="relative">
        <input
          ref={ref}
          className={`field py-1 pr-6 font-mono text-[11.5px] ${state.status === "invalid" ? "border-danger/60" : ""}`}
          placeholder="Tables (optional): schema.table, schema.table.column, a & b, a or b"
          aria-label="Note target"
          aria-autocomplete="list"
          aria-expanded={open && items.length > 0}
          aria-invalid={state.status === "invalid"}
          role="combobox"
          value={value}
          onChange={(e) => {
            onChange(e.target.value);
            setCursor(e.target.selectionStart ?? e.target.value.length);
            setOpen(true);
            setActive(0);
          }}
          onSelect={(e) => setCursor((e.target as HTMLInputElement).selectionStart ?? 0)}
          onFocus={() => setOpen(true)}
          onBlur={() => setTimeout(() => setOpen(false), 120)}
          onKeyDown={(e) => {
            if (!open || items.length === 0) {
              if (e.key === "ArrowDown") setOpen(true);
              return;
            }
            if (e.key === "ArrowDown" || e.key === "ArrowUp") {
              e.preventDefault();
              setActive((a) => (a + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length);
            } else if (e.key === "Enter" || e.key === "Tab") {
              e.preventDefault();
              accept(items[Math.min(active, items.length - 1)]);
            } else if (e.key === "Escape") {
              e.stopPropagation();
              setOpen(false);
            }
          }}
        />
        <span className="pointer-events-none absolute right-1.5 top-1/2 -translate-y-1/2">
          {state.status === "checking" && <Loader2 size={12} className="animate-spin text-muted" />}
          {state.status === "ok" && <Check size={12} className="text-success" />}
          {state.status === "invalid" && <TriangleAlert size={12} className="text-danger" />}
        </span>
      </div>
      {open && items.length > 0 && (
        <ul role="listbox" className="absolute z-30 mt-0.5 max-h-56 w-full overflow-auto rounded-md border border-line bg-panel py-0.5 shadow-lg">
          {items.map((s, i) => (
            <li
              key={s.insert}
              role="option"
              aria-selected={i === active}
              className={`flex cursor-pointer items-center gap-1.5 px-2 py-0.5 text-[12px] ${i === active ? "bg-hover" : ""}`}
              onMouseDown={(e) => {
                e.preventDefault();
                accept(s);
              }}
              onMouseEnter={() => setActive(i)}
            >
              {s.kind === "table" ? <Table2 size={12} className="shrink-0 text-muted" /> : <Columns3 size={12} className="shrink-0 text-muted" />}
              <span className="min-w-0 truncate font-mono">{s.label}</span>
              {s.detail && <span className="ml-auto shrink-0 truncate font-mono text-[10.5px] text-muted">{s.detail}</span>}
            </li>
          ))}
        </ul>
      )}
      {state.status === "invalid" && <div className="mt-0.5 text-[11px] text-danger">{state.error}</div>}
      {state.status === "ok" && state.canonical && state.canonical !== value.trim() && (
        <div className="mt-0.5 text-[11px] text-muted">
          Saved as <span className="font-mono">{state.canonical}</span>
        </div>
      )}
    </div>
  );
}
