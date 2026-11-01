import { useState } from "react";
import { Bookmark, BookmarkPlus, ChevronDown, Trash2 } from "lucide-react";
import { applySaved, isEmptyView, tableLabel, toSaved, viewsFor } from "../lib/gridViews";
import type { ResultInfo, ViewSpec } from "../lib/types";
import { filterLabel, uid } from "../lib/util";
import { useStore } from "../store";
import { MenuSeparator, Popover } from "./ui";

/**
 * Saved grid views (filters, sort, quick filter by column name): apply one,
 * save the current view (for this table or any result with its columns),
 * delete one.
 */
export function GridViewsMenu({ info, view, sourceKey, onApply }: { info: ResultInfo; view: ViewSpec; sourceKey: string | null; onApply: (v: ViewSpec) => void }) {
  const views = useStore((s) => s.gridViews);
  const saveView = useStore((s) => s.saveGridView);
  const deleteView = useStore((s) => s.deleteGridView);
  const toast = useStore((s) => s.toast);
  const [at, setAt] = useState<{ x: number; y: number } | null>(null);
  const [name, setName] = useState("");
  const [forTable, setForTable] = useState(true);
  const columns = info.columns.map((c) => c.name);
  const offered = viewsFor(views, sourceKey, columns);
  const empty = isEmptyView(view);
  const close = () => {
    setAt(null);
    setName("");
  };
  const save = () => {
    if (!name.trim() || empty) return;
    const table = sourceKey && forTable ? sourceKey : null;
    // Same name in the same place: replaced.
    const same = views.find((v) => v.table === table && v.name.toLowerCase() === name.trim().toLowerCase());
    saveView(toSaved(view, columns, name, table, same?.id ?? uid()));
    toast(`Saved view "${name.trim()}"`, "success");
    close();
  };
  const describe = (v: (typeof offered)[number]) =>
    [
      ...v.filters.map((f) => filterLabel({ column: 0, op: f.op, value: f.value }, f.column)),
      ...v.sort.map((s) => `${s.column} ${s.descending ? "↓" : "↑"}`),
      ...(v.quick ? [`"${v.quick}"`] : []),
    ].join(", ");
  return (
    <>
      <button
        className={`btn-ghost shrink-0 whitespace-nowrap border border-line py-1 ${at ? "bg-hover" : ""}`}
        aria-haspopup="menu"
        aria-label="Saved views"
        title="Saved views: filters, sort and quick filter, by column name"
        onClick={(e) => setAt({ x: e.clientX, y: e.clientY + 10 })}
      >
        <Bookmark size={13} /> <span className="hidden @3xl:inline">Views</span>
        {offered.length > 0 && <span className="text-[10.5px] text-muted">{offered.length}</span>}
        <ChevronDown size={11} />
      </button>
      {at && (
        <Popover x={Math.max(8, at.x - 280)} y={at.y} onClose={close} className="w-80">
          {offered.length === 0 && <div className="px-2 py-1.5 text-[12px] text-muted">No saved views for these columns yet.</div>}
          {offered.map((v) => (
            <div key={v.id} className="group flex items-center gap-1 rounded-md hover:bg-hover">
              <button
                className="min-w-0 flex-1 px-2 py-1.5 text-left"
                title={describe(v)}
                onClick={() => {
                  const r = applySaved(v, columns);
                  onApply(r.view);
                  if (r.missing.length) toast(`View "${v.name}": no column ${r.missing.join(", ")} in this result (skipped)`, "info");
                  close();
                }}
              >
                <div className="truncate text-[13px]">{v.name}</div>
                <div className="truncate text-[11px] text-muted">
                  {v.table ? tableLabel(v.table) : "any result"} · {describe(v) || "no filters"}
                </div>
              </button>
              <button
                className="icon-btn mr-1 h-6 w-6 shrink-0 opacity-0 group-hover:opacity-100 focus:opacity-100"
                aria-label={`Delete view ${v.name}`}
                title="Delete"
                onClick={() => deleteView(v.id)}
              >
                <Trash2 size={12} />
              </button>
            </div>
          ))}
          <MenuSeparator />
          <div className="space-y-1.5 px-2 py-1">
            <div className="flex items-center gap-1.5 text-[12px] font-medium">
              <BookmarkPlus size={13} className="text-muted" /> Save the current view
            </div>
            <input
              className="field w-full py-1"
              placeholder={empty ? "Filter or sort the rows first" : "Name"}
              aria-label="View name"
              disabled={empty}
              autoFocus={!empty}
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && save()}
            />
            {sourceKey && (
              <label className="flex items-center gap-1.5 text-[11.5px] text-muted">
                <input type="checkbox" checked={forTable} disabled={empty} onChange={(e) => setForTable(e.target.checked)} /> Only for {tableLabel(sourceKey)}
              </label>
            )}
            <button className="btn-primary w-full justify-center py-1" disabled={empty || !name.trim()} onClick={save}>
              Save view
            </button>
          </div>
        </Popover>
      )}
    </>
  );
}
