import { useEffect, useMemo, useState } from "react";
import { ChevronDown, ChevronRight, ClipboardCopy, WrapText, X } from "lucide-react";
import { hexDump, hexToDataUrl, imageType, parseJson, prettyXml, valueKinds, valueSize, type ValueKind } from "../lib/valueView";
import { copyText } from "./CatalogMenus";

export interface FocusedCell {
  col: number;
  row: number;
  column: string;
  family?: string;
  /** null = SQL NULL; undefined = not loaded yet. */
  value: string | null | undefined;
  /** The cell can be changed (grid editing on, a table column). */
  editable: boolean;
}

const KIND_LABEL: Record<ValueKind, string> = { text: "Text", json: "JSON", xml: "XML", hex: "Hex", image: "Image" };

/**
 * The focused cell's whole value beside the grid: JSON as a tree or
 * indented text, XML indented, binary as a hex dump or image, long text
 * wrapped. Editable cells can be changed here (pending like grid edits).
 */
export function ValuePanel({ cell, onClose, onEdit }: { cell: FocusedCell | null; onClose: () => void; onEdit?: (value: string | null) => void }) {
  const v = cell?.value;
  const kinds = useMemo(() => (typeof v === "string" ? valueKinds(v, cell?.family) : (["text"] as ValueKind[])), [v, cell?.family]);
  const [kind, setKind] = useState<ValueKind>(kinds[0]);
  const [wrap, setWrap] = useState(true);
  const [draft, setDraft] = useState<string | null>(null);
  // A new cell: its best form, and no draft.
  useEffect(() => {
    setKind(kinds[0]);
    setDraft(null);
  }, [cell?.col, cell?.row, kinds]);

  const shown = useMemo(() => {
    if (typeof v !== "string") return "";
    if (kind === "json") return JSON.stringify(parseJson(v), null, 2);
    if (kind === "xml") return prettyXml(v);
    if (kind === "hex") return hexDump(v);
    return v;
  }, [v, kind]);

  return (
    <aside className="flex w-96 shrink-0 flex-col border-l border-line bg-panel" aria-label="Cell value">
      <div className="flex h-9 shrink-0 items-center gap-1.5 border-b border-line px-2 text-[12px]">
        <span className="min-w-0 flex-1 truncate font-medium" title={cell?.column}>
          {cell ? `${cell.column} · row ${(cell.row + 1).toLocaleString()}` : "Value"}
        </span>
        {kinds.length > 1 && (
          <div className="flex rounded-md border border-line p-0.5" role="radiogroup" aria-label="Show as">
            {kinds.map((k) => (
              <button key={k} role="radio" aria-checked={kind === k} onClick={() => setKind(k)} className={`h-5 rounded px-1.5 text-[11px] ${kind === k ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}>
                {KIND_LABEL[k]}
              </button>
            ))}
          </div>
        )}
        <button className={`icon-btn h-6 w-6 ${wrap ? "text-accent" : ""}`} aria-pressed={wrap} title="Wrap long lines" aria-label="Wrap long lines" onClick={() => setWrap(!wrap)}>
          <WrapText size={13} />
        </button>
        <button className="icon-btn h-6 w-6" title="Copy the value" aria-label="Copy value" disabled={typeof v !== "string"} onClick={() => typeof v === "string" && void copyText(kind === "json" || kind === "xml" ? shown : v, "value")}>
          <ClipboardCopy size={13} />
        </button>
        <button className="icon-btn h-6 w-6" aria-label="Close value panel" onClick={onClose}>
          <X size={13} />
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-2">
        {!cell ? (
          <p className="p-2 text-[12px] text-muted">Select a cell to see its whole value.</p>
        ) : v === undefined ? (
          <p className="p-2 text-[12px] text-muted">Loading…</p>
        ) : v === null && draft === null ? (
          <p className="p-2 font-mono text-[12px] text-muted">NULL</p>
        ) : draft !== null ? (
          <textarea className="field h-full min-h-40 w-full resize-none font-mono text-[12px]" aria-label="New value" value={draft} onChange={(e) => setDraft(e.target.value)} autoFocus />
        ) : kind === "image" ? (
          <img src={hexToDataUrl(v!, imageType(v!) ?? "image/png")} alt={`${cell.column} image`} className="max-w-full rounded border border-line" />
        ) : kind === "json" ? (
          <JsonTree value={parseJson(v!)} wrap={wrap} />
        ) : (
          <pre className={`font-mono text-[12px] ${wrap ? "whitespace-pre-wrap break-all" : "whitespace-pre"}`}>{shown}</pre>
        )}
      </div>
      {cell && typeof v !== "undefined" && (
        <div className="flex h-8 shrink-0 items-center gap-2 border-t border-line px-2 text-[11.5px] text-muted">
          <span className="min-w-0 flex-1 truncate">{typeof v === "string" ? valueSize(v, cell.family) : "NULL"}</span>
          {cell.editable && onEdit && cell.family !== "binary" && (
            draft === null ? (
              <button className="btn-ghost h-6 py-0 text-[11.5px]" onClick={() => setDraft(v ?? "")}>
                Edit
              </button>
            ) : (
              <>
                <button className="btn-ghost h-6 py-0 text-[11.5px]" onClick={() => setDraft(null)}>
                  Cancel
                </button>
                <button className="btn-ghost h-6 py-0 text-[11.5px]" title="Set the cell to NULL" onClick={() => (onEdit(null), setDraft(null))}>
                  NULL
                </button>
                <button className="btn-primary h-6 py-0 text-[11.5px]" title="Keep as a pending change (Save in the editing bar writes it)" onClick={() => (onEdit(draft), setDraft(null))}>
                  Apply
                </button>
              </>
            )
          )}
        </div>
      )}
    </aside>
  );
}

/** Collapsible JSON: objects and arrays open two levels deep. */
function JsonTree({ value, wrap }: { value: unknown; wrap: boolean }) {
  return (
    <div className={`font-mono text-[12px] ${wrap ? "break-all" : "whitespace-nowrap"}`} role="tree" aria-label="JSON">
      <JsonNode name={null} value={value} depth={0} />
    </div>
  );
}

function JsonNode({ name, value, depth }: { name: string | null; value: unknown; depth: number }) {
  const [open, setOpen] = useState(depth < 2);
  const label = name !== null ? <span className="text-accent">{name}: </span> : null;
  if (value === null || typeof value !== "object") {
    const cls = typeof value === "string" ? "text-success" : typeof value === "number" ? "text-warning" : "text-muted";
    return (
      <div role="treeitem" style={{ paddingLeft: depth * 14 }}>
        {label}
        <span className={cls}>{typeof value === "string" ? JSON.stringify(value) : String(value)}</span>
      </div>
    );
  }
  const entries = Array.isArray(value) ? value.map((v, i) => [String(i), v] as const) : Object.entries(value as Record<string, unknown>);
  const [o, c] = Array.isArray(value) ? ["[", "]"] : ["{", "}"];
  return (
    <div role="treeitem" aria-expanded={open}>
      <button className="flex items-center text-left hover:bg-hover" style={{ paddingLeft: depth * 14 }} onClick={() => setOpen(!open)}>
        {open ? <ChevronDown size={11} className="shrink-0" /> : <ChevronRight size={11} className="shrink-0" />}
        {label}
        <span className="text-muted">
          {o}
          {!open && ` ${entries.length} ${Array.isArray(value) ? "items" : "keys"} ${c}`}
        </span>
      </button>
      {open && (
        <>
          {entries.slice(0, 2000).map(([k, v]) => (
            <JsonNode key={k} name={Array.isArray(value) ? null : k} value={v} depth={depth + 1} />
          ))}
          {entries.length > 2000 && <div style={{ paddingLeft: (depth + 1) * 14 }} className="text-muted">… {entries.length - 2000} more</div>}
          <div style={{ paddingLeft: depth * 14 + 11 }} className="text-muted">
            {c}
          </div>
        </>
      )}
    </div>
  );
}
