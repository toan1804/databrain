import { useMemo, useState } from "react";
import { AlertTriangle, ChevronDown, ChevronRight, Library, Search } from "lucide-react";
import { useAi } from "../aiStore";
import { DEFAULT_BATCH, MAX_BATCH, buildScope, estimateQueries, initialSelection, isBulk, planCatalogs, selectedObjects } from "../lib/indexScope";
import { schemaLabel } from "../lib/catalog";
import { formatCount } from "../lib/util";
import { useStore } from "../store";
import { Modal } from "./ui";

/** Max schema rows rendered at once (filter to narrow down). */
const MAX_ROWS = 600;

/** Asks which catalogs/schemas to index before a large knowledge index run. */
export function IndexScopeDialog() {
  const picker = useAi((s) => s.indexPicker);
  if (!picker) return null;
  return <ScopeDialog key={picker.connectionId} />;
}

function ScopeDialog() {
  const { connectionId, plan } = useAi((s) => s.indexPicker)!;
  const close = useAi((s) => s.closeIndexPicker);
  const index = useAi((s) => s.indexKnowledge);
  const conn = useStore((s) => s.connections.find((c) => c.id === connectionId));
  const [selected, setSelected] = useState<Set<string>>(() => initialSelection(plan));
  // Expanded catalogs without a filter; collapsed ones while filtering
  // (matches start expanded but can still be collapsed).
  const [open, setOpen] = useState<Set<string>>(() => new Set());
  const [closedInFilter, setClosedInFilter] = useState<Set<string>>(() => new Set());
  const [batchText, setBatchText] = useState(String(plan.batch || DEFAULT_BATCH));
  const batchNum = Number(batchText);
  const batchOk = Number.isInteger(batchNum) && batchNum >= 1 && batchNum <= MAX_BATCH;
  const batch = batchOk ? batchNum : plan.batch || DEFAULT_BATCH;
  const [filter, setFilter] = useState("");
  const [showSystem, setShowSystem] = useState(false);

  const catalogs = useMemo(() => planCatalogs(plan, showSystem), [plan, showSystem]);
  const threeLevel = catalogs.some((c) => c.name);
  const q = filter.trim().toLowerCase();
  const shown = useMemo(
    () =>
      catalogs
        .map((c) => ({ ...c, schemas: q && !c.name.toLowerCase().includes(q) ? c.schemas.filter((s) => s.name.toLowerCase().includes(q)) : c.schemas }))
        .filter((c) => c.schemas.length > 0),
    [catalogs, q],
  );

  const kind = conn?.config.kind ?? "postgres";
  const queries = estimateQueries(kind, plan, selected, batch);
  const tables = selectedObjects(plan, selected);
  const scope = buildScope(plan, selected);

  const toggle = (names: string[], on: boolean) =>
    setSelected((prev) => {
      const next = new Set(prev);
      for (const n of names) (on ? next.add(n) : next.delete(n));
      return next;
    });
  const visibleNames = shown.flatMap((c) => c.schemas.map((s) => s.name));

  let rows = 0;
  return (
    <Modal
      title={`Choose what to index${conn ? ` · ${conn.name}` : ""}`}
      onClose={close}
      width={640}
      footer={
        <>
          <span className="mr-auto text-[11.5px] text-muted">
            {selected.size} schema{selected.size === 1 ? "" : "s"}
            {tables !== null ? ` · ${formatCount(tables)} tables` : ""} · about {formatCount(queries)} metadata quer{queries === 1 ? "y" : "ies"}
          </span>
          <button className="btn-ghost" onClick={close}>
            Cancel
          </button>
          <button
            className="btn-primary"
            disabled={selected.size === 0 || !batchOk}
            onClick={() => void index(connectionId, { scope, batch: batch !== plan.batch ? batch : undefined })}
          >
            Index {selected.size === plan.schemas.length ? "all" : formatCount(selected.size)}
          </button>
        </>
      }
    >
      <div className="space-y-2.5 text-[12.5px]">
        <div className="flex items-start gap-2 rounded-lg border border-warning/40 bg-warning/5 p-2.5">
          <AlertTriangle size={14} className="mt-0.5 shrink-0 text-warning" />
          <div>
            This connection has{" "}
            {threeLevel && (
              <>
                <b>{formatCount(plan.catalogs)}</b> {kind === "bigquery" ? "projects" : kind === "databricks" ? "catalogs" : "databases"},{" "}
              </>
            )}
            <b>{formatCount(plan.schemas.filter((s) => !s.system).length)}</b> schemas
            {plan.total_objects !== null && (
              <>
                {" "}
                and <b>{formatCount(plan.total_objects)}</b> tables
              </>
            )}
            . Indexing reads metadata only (names, types, comments), but every schema costs queries on the server. Pick what the assistant should know
            about; your choice is saved and used when you re-index.
          </div>
        </div>

        <div className="flex items-center gap-2">
          <div className="relative flex-1">
            <Search size={13} className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-muted" />
            <input
              className="field py-1 pl-7"
              placeholder={threeLevel ? "Filter catalogs and schemas" : "Filter schemas"}
              aria-label="Filter catalogs and schemas"
              value={filter}
              autoFocus
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              onChange={(e) => setFilter(e.target.value)}
            />
          </div>
          <button className="btn-ghost border border-line py-1" onClick={() => toggle(visibleNames, true)}>
            {q ? "Select shown" : "All"}
          </button>
          <button className="btn-ghost border border-line py-1" onClick={() => (q ? toggle(visibleNames, false) : setSelected(new Set()))}>
            {q ? "Clear shown" : "None"}
          </button>
        </div>
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5 text-[11.5px] text-muted">
          <label className="flex items-center gap-1.5">
            <input type="checkbox" checked={showSystem} onChange={(e) => setShowSystem(e.target.checked)} /> Show system schemas
          </label>
          <label className="flex items-center gap-1.5" title={isBulk(kind) ? "Schemas of a catalog fetched together: tables + columns in 2 queries per batch (Databricks reads at most 200 schemas per query)." : "Progress and Cancel apply between batches. This database is read one schema at a time (2 queries per schema)."}>
            Schemas per batch
            <input
              type="number"
              inputMode="numeric"
              min={1}
              max={MAX_BATCH}
              className={`field h-6 w-20 py-0 ${batchOk ? "" : "border-danger"}`}
              aria-label="Schemas per batch"
              aria-invalid={!batchOk}
              value={batchText}
              onChange={(e) => setBatchText(e.target.value)}
            />
            <span>{batchOk ? (isBulk(kind) ? "bigger = fewer queries" : "") : `1–${MAX_BATCH}`}</span>
          </label>
        </div>

        <div className="max-h-[46vh] overflow-auto rounded-lg border border-line p-1" role="tree" aria-label="Catalogs and schemas">
          {shown.length === 0 && <div className="p-3 text-center text-muted">Nothing matches.</div>}
          {shown.map((c) => {
            const names = c.schemas.map((s) => s.name);
            const n = names.filter((x) => selected.has(x)).length;
            const isOpen = !c.name || (q ? !closedInFilter.has(c.name) : open.has(c.name));
            const objs = c.schemas.reduce<number | null>((t, s) => (t === null || s.objects === null ? null : t + s.objects), 0);
            const head = c.name ? (
              <div key={`c:${c.name}`} role="treeitem" aria-expanded={isOpen} className="flex h-7 items-center gap-1.5 rounded-md px-1 hover:bg-hover">
                <button
                  className="flex h-5 w-5 items-center justify-center text-muted"
                  aria-label={isOpen ? "Collapse" : "Expand"}
                  onClick={() => {
                    const flip = (o: Set<string>) => {
                      const x = new Set(o);
                      if (x.has(c.name)) x.delete(c.name);
                      else x.add(c.name);
                      return x;
                    };
                    if (q) setClosedInFilter(flip);
                    else setOpen(flip);
                  }}
                >
                  {isOpen ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
                </button>
                <input
                  type="checkbox"
                  aria-label={`Index catalog ${c.name}`}
                  checked={n === names.length}
                  ref={(el) => {
                    if (el) el.indeterminate = n > 0 && n < names.length;
                  }}
                  onChange={(e) => toggle(names, e.target.checked)}
                />
                <Library size={13} className="text-muted" />
                <span className="min-w-0 flex-1 truncate font-medium">{c.name}</span>
                <span className="shrink-0 text-[11px] text-muted">
                  {n > 0 && n < names.length ? `${n}/` : ""}
                  {names.length} schemas{objs !== null ? ` · ${formatCount(objs)} tables` : ""}
                </span>
              </div>
            ) : null;
            const body =
              isOpen &&
              c.schemas.map((s) => {
                if (++rows > MAX_ROWS) return null;
                return (
                  <label
                    key={s.name}
                    role="treeitem"
                    className="flex h-6 cursor-pointer items-center gap-1.5 rounded-md pr-1 hover:bg-hover"
                    style={{ paddingLeft: c.name ? 36 : 8 }}
                  >
                    <input type="checkbox" checked={selected.has(s.name)} onChange={(e) => toggle([s.name], e.target.checked)} />
                    <span className="min-w-0 flex-1 truncate">
                      {s.catalog ? schemaLabel(s) : s.name}
                      {s.is_default && <span className="ml-1.5 text-[10.5px] text-accent">default</span>}
                      {s.system && <span className="ml-1.5 text-[10.5px] text-muted">system</span>}
                    </span>
                    {s.objects !== null && <span className="shrink-0 text-[11px] text-muted">{formatCount(s.objects)}</span>}
                  </label>
                );
              });
            return (
              <div key={c.name || "_"}>
                {head}
                {body}
              </div>
            );
          })}
          {rows > MAX_ROWS && <div className="p-2 text-center text-[11px] text-muted">{formatCount(rows - MAX_ROWS)} more schemas. Use the filter to narrow down.</div>}
        </div>
      </div>
    </Modal>
  );
}
