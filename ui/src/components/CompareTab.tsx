import { useEffect, useMemo, useState } from "react";
import { ArrowRight, ChevronDown, ChevronRight, ClipboardCopy, FileCode2, GitCompare, Loader2, Minus, Pencil, Plus } from "lucide-react";
import { api, toError } from "../lib/api";
import { traitsOf } from "../lib/sourceTraits";
import type { DiffStatus, ObjectDiff, SchemaDiff, SchemaInfo } from "../lib/types";
import { useStore, type CompareRef } from "../store";
import { copyText } from "./CatalogMenus";

type Side = NonNullable<CompareRef["source"]>;

/** Open a schema compare tab (`source` filled in when started from a schema). */
export function openCompare(source?: Side) {
  const st = useStore.getState();
  st.newTab({ title: "Compare schemas", sql: "", compare: { source: source ?? null, target: null }, connection_id: source?.connection_id ?? null });
}

const STATUS: Record<DiffStatus, { label: string; cls: string; icon: React.ReactNode }> = {
  only_source: { label: "Only in source", cls: "text-success", icon: <Plus size={12} /> },
  only_target: { label: "Only in target", cls: "text-danger", icon: <Minus size={12} /> },
  changed: { label: "Different", cls: "text-warning", icon: <Pencil size={12} /> },
  same: { label: "Same", cls: "text-muted", icon: null },
};

const WHAT: Record<string, string> = {
  added: "column only in source",
  removed: "column only in target",
  type: "type",
  nullable: "NULL",
  default: "default",
  primary_key: "primary key",
  foreign_key: "foreign key",
};

function SidePicker({ label, side, onChange }: { label: string; side: Side | null; onChange: (s: Side | null) => void }) {
  const connections = useStore((s) => s.connections);
  const [schemas, setSchemas] = useState<SchemaInfo[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const connId = side?.connection_id ?? "";
  useEffect(() => {
    setSchemas(null);
    setErr(null);
    if (!connId) return;
    let alive = true;
    api.listSchemas(connId).then(
      (s) => {
        if (!alive) return;
        setSchemas(s);
        // Pick the default schema when none is chosen yet.
        if (side && !side.schema) {
          const d = s.find((x) => x.is_default) ?? s[0];
          if (d) onChange({ connection_id: connId, schema: d.name });
        }
      },
      (e) => alive && setErr(toError(e).message),
    );
    return () => {
      alive = false;
    };
  }, [connId]); // eslint-disable-line react-hooks/exhaustive-deps
  const sql = connections.filter((c) => traitsOf(c.config.kind).language === "sql");
  return (
    <fieldset className="flex min-w-0 flex-1 flex-col gap-1">
      <legend className="mb-1 text-[11px] uppercase tracking-wide text-muted">{label}</legend>
      <select className="field py-1" aria-label={`${label} connection`} value={connId} onChange={(e) => onChange(e.target.value ? { connection_id: e.target.value, schema: "" } : null)}>
        <option value="">Choose a connection…</option>
        {sql.map((c) => (
          <option key={c.id} value={c.id}>
            {c.name}
          </option>
        ))}
      </select>
      <select className="field py-1" aria-label={`${label} schema`} disabled={!schemas} value={side?.schema ?? ""} onChange={(e) => side && onChange({ ...side, schema: e.target.value })}>
        {!schemas && <option value="">{connId ? (err ? "Can't list schemas" : "Loading…") : "Schema"}</option>}
        {schemas?.map((s) => (
          <option key={s.name} value={s.name}>
            {s.name}
          </option>
        ))}
      </select>
      {err && <span className="text-[11px] text-danger">{err}</span>}
    </fieldset>
  );
}

export function CompareTab({ tabId, visible }: { tabId: string; visible: boolean }) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const updateTab = useStore((s) => s.updateTab);
  const connections = useStore((s) => s.connections);
  const ref = tab?.compare;
  const [diff, setDiff] = useState<SchemaDiff | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showSame, setShowSame] = useState(false);
  const [open, setOpen] = useState<Set<string>>(new Set());

  const ready = !!ref?.source?.schema && !!ref?.target?.schema;
  const run = async () => {
    if (!ref?.source || !ref.target) return;
    setBusy(true);
    setError(null);
    try {
      const d = await api.schemaDiff(ref.source, ref.target);
      setDiff(d);
      setOpen(new Set(d.objects.filter((o) => o.status === "changed").slice(0, 20).map((o) => o.name)));
    } catch (e) {
      setError(toError(e).message);
    } finally {
      setBusy(false);
    }
  };

  const counts = useMemo(() => {
    const c: Record<DiffStatus, number> = { only_source: 0, only_target: 0, changed: 0, same: 0 };
    diff?.objects.forEach((o) => c[o.status]++);
    return c;
  }, [diff]);

  if (!tab || !ref) return null;
  const set = (patch: Partial<CompareRef>) => {
    updateTab(tab.id, { compare: { ...ref, ...patch } });
    setDiff(null);
  };
  const name = (s: Side | null) => (s ? `${connections.find((c) => c.id === s.connection_id)?.name ?? "?"} · ${s.schema}` : "");
  const shown = diff?.objects.filter((o) => showSame || o.status !== "same") ?? [];
  const openScript = () => {
    if (!diff || !ref.target) return;
    const st = useStore.getState();
    st.newTab({ title: `Migrate ${ref.target.schema}`, sql: diff.script, connection_id: ref.target.connection_id });
  };

  return (
    <div className="flex h-full min-h-0 flex-col" style={{ display: visible ? "flex" : "none" }}>
      <div className="flex shrink-0 items-end gap-3 border-b border-line p-3 text-[12.5px]">
        <SidePicker label="Source (as it should be)" side={ref.source} onChange={(source) => set({ source })} />
        <button className="icon-btn mb-1" title="Swap source and target" aria-label="Swap source and target" onClick={() => set({ source: ref.target, target: ref.source })}>
          <ArrowRight size={15} />
        </button>
        <SidePicker label="Target (to change)" side={ref.target} onChange={(target) => set({ target })} />
        <button className="btn-primary mb-0.5 shrink-0" disabled={!ready || busy} onClick={() => void run()}>
          {busy ? <Loader2 size={13} className="animate-spin" /> : <GitCompare size={13} />} Compare
        </button>
      </div>
      {error && (
        <p className="p-3 text-[12.5px] text-danger" role="alert">
          {error}
        </p>
      )}
      {!diff && !error && <p className="p-4 text-[12.5px] text-muted">{busy ? "Reading both schemas…" : "Choose two schemas and Compare. Nothing is changed: the script opens in an editor for you to review and run."}</p>}
      {diff && (
        <div className="flex min-h-0 flex-1">
          <div className="flex min-h-0 min-w-0 flex-1 flex-col">
            <div className="flex h-8 shrink-0 items-center gap-3 border-b border-line bg-panel-2 px-3 text-[11.5px]" aria-live="polite">
              {(["only_source", "only_target", "changed"] as const).map((s) => (
                <span key={s} className={STATUS[s].cls}>
                  {STATUS[s].label}: {counts[s]}
                </span>
              ))}
              <label className="ml-auto flex items-center gap-1 text-muted">
                <input type="checkbox" checked={showSame} onChange={(e) => setShowSame(e.target.checked)} /> Same ({counts.same})
              </label>
            </div>
            {diff.source_kind !== diff.target_kind && (
              <p className="border-b border-line bg-warning/10 px-3 py-1 text-[11.5px] text-warning">
                {traitsOf(diff.source_kind).name} vs {traitsOf(diff.target_kind).name}: types are compared as each database writes them.
              </p>
            )}
            <div className="min-h-0 flex-1 overflow-auto" role="tree" aria-label="Differences">
              {shown.length === 0 && <p className="p-4 text-[12.5px] text-muted">The schemas are the same.</p>}
              {shown.map((o) => (
                <DiffRow key={o.name} o={o} open={open.has(o.name)} onToggle={() => setOpen((s) => { const n = new Set(s); if (n.has(o.name)) n.delete(o.name); else n.add(o.name); return n; })} />
              ))}
            </div>
          </div>
          <div className="flex w-[44%] shrink-0 flex-col border-l border-line">
            <div className="flex h-8 shrink-0 items-center gap-2 border-b border-line px-2 text-[11.5px]">
              <span className="min-w-0 flex-1 truncate text-muted" title={name(ref.target)}>
                Script for {name(ref.target)}
              </span>
              <button className="btn-ghost h-6 py-0 text-[11.5px]" onClick={() => void copyText(diff.script, "script")}>
                <ClipboardCopy size={12} /> Copy
              </button>
              <button className="btn-ghost h-6 py-0 text-[11.5px]" title="Open in a query tab on the target connection (to review and run)" onClick={openScript}>
                <FileCode2 size={12} /> Open in editor
              </button>
            </div>
            <pre className="min-h-0 flex-1 overflow-auto p-2 font-mono text-[11.5px]">{diff.script}</pre>
          </div>
        </div>
      )}
    </div>
  );
}

function DiffRow({ o, open, onToggle }: { o: ObjectDiff; open: boolean; onToggle: () => void }) {
  const s = STATUS[o.status];
  return (
    <div role="treeitem" aria-expanded={o.changes.length ? open : undefined} className="border-b border-line/50 text-[12px]">
      <button className="flex w-full items-center gap-2 px-3 py-1 text-left hover:bg-hover" onClick={onToggle} disabled={!o.changes.length}>
        {o.changes.length ? open ? <ChevronDown size={12} /> : <ChevronRight size={12} /> : <span className="w-3" />}
        <span className={`flex w-4 justify-center ${s.cls}`}>{s.icon}</span>
        <span className="font-mono">{o.name}</span>
        <span className="text-[11px] text-muted">{o.kind.replace("_", " ")}</span>
        <span className={`ml-auto text-[11px] ${s.cls}`}>{o.changes.length ? `${o.changes.length} difference${o.changes.length === 1 ? "" : "s"}` : s.label}</span>
      </button>
      {open && o.changes.length > 0 && (
        <table className="mb-1 ml-12 text-[11.5px]">
          <tbody>
            {o.changes.map((c, i) => (
              <tr key={i}>
                <td className="pr-3 font-mono">{c.column ?? ""}</td>
                <td className="pr-3 text-muted">{WHAT[c.what] ?? c.what}</td>
                <td className="pr-2 font-mono text-success">{c.source ?? "—"}</td>
                <td className="pr-2 text-muted">vs</td>
                <td className="font-mono text-danger">{c.target ?? "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
