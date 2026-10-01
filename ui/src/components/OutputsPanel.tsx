import { useMemo, useState } from "react";
import { AlertTriangle, Eraser, GitCompare, HardDrive, Layers, Pin, Search, TerminalSquare, Trash2 } from "lucide-react";
import type { OutputInfo } from "../lib/types";
import { formatBytes, formatCount, sqlPreview } from "../lib/util";
import { useStore } from "../store";
import { dropOutput, dropUnpinnedOutputs, formatAge, openOutput, openResultsQuery, outputRef } from "../outputs";
import { CompareDialog, OutputChip } from "./OutputChip";
import { ConnDot } from "./ui";

type Filter = "all" | "named" | "pinned";

export function OutputsPanel() {
  const outputs = useStore((s) => s.outputs);
  const connections = useStore((s) => s.connections);
  const [q, setQ] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const [compare, setCompare] = useState(false);

  const shown = useMemo(() => {
    const t = q.toLowerCase().trim();
    return outputs.filter((o) => {
      if (filter === "named" && !o.name && !o.version_of) return false;
      if (filter === "pinned" && !o.pinned) return false;
      if (!t) return true;
      return `${o.handle} ${o.name ?? ""} ${o.connection_name} ${o.sql}`.toLowerCase().includes(t);
    });
  }, [outputs, q, filter]);

  const live = outputs.filter((o) => o.state === "live").reduce((n, o) => n + o.bytes, 0);
  const starter = () => {
    const named = outputs.filter((o) => o.state !== "evicted").slice(0, 2);
    const sql = named.length
      ? `-- Outputs are tables in the results schema; join across databases.\nSELECT *\nFROM ${outputRef(named[0])}\nLIMIT 100;`
      : "-- Run a query first; its output becomes results.r1, results.r2, …\nSELECT 1;";
    void openResultsQuery(sql, "Results", false);
  };

  return (
    <>
      <div className="flex h-10 shrink-0 items-center justify-between px-3">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-muted">Outputs</span>
        <div className="flex items-center gap-0.5">
          <button className="icon-btn" title="Query outputs with SQL (DuckDB)" aria-label="Query outputs" onClick={starter}>
            <TerminalSquare size={14} />
          </button>
          <button className="icon-btn" title="Compare two outputs" aria-label="Compare outputs" disabled={outputs.length < 2} onClick={() => setCompare(true)}>
            <GitCompare size={14} />
          </button>
          <button
            className="icon-btn"
            title="Drop unpinned outputs (keeps pinned ones and each tab's latest result)"
            aria-label="Drop unpinned outputs"
            disabled={!outputs.some((o) => !o.pinned && !o.active)}
            onClick={dropUnpinnedOutputs}
          >
            <Eraser size={14} />
          </button>
        </div>
      </div>
      <div className="space-y-1.5 px-2 pb-2">
        <div className="relative">
          <Search size={13} className="absolute left-2 top-1/2 -translate-y-1/2 text-muted" />
          <input className="field py-1 pl-7" placeholder="Search outputs" aria-label="Search outputs" value={q} onChange={(e) => setQ(e.target.value)} />
        </div>
        <div className="flex gap-1 text-[11.5px]" role="radiogroup" aria-label="Filter outputs">
          {(["all", "named", "pinned"] as Filter[]).map((f) => (
            <button
              key={f}
              role="radio"
              aria-checked={filter === f}
              onClick={() => setFilter(f)}
              className={`rounded-md px-2 py-0.5 capitalize ${filter === f ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
            >
              {f}
            </button>
          ))}
          <span className="ml-auto self-center text-[10.5px] text-muted" title="Memory used by outputs">
            {formatBytes(live)}
          </span>
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-3">
        {shown.length === 0 && (
          <div className="px-3 py-8 text-center text-[12.5px] text-muted">
            <Layers size={26} className="mx-auto mb-3 opacity-50" />
            {outputs.length === 0 ? (
              <>
                Every query result gets a handle like <span className="font-mono">r12</span>. Name or pin outputs, query them together as{" "}
                <span className="font-mono">results.&lt;name&gt;</span>, compare versions, or @mention them to the AI.
              </>
            ) : (
              "No matches"
            )}
          </div>
        )}
        {shown.map((o) => (
          <OutputRow key={o.handle} o={o} color={connections.find((c) => c.id === o.connection_id)?.color} />
        ))}
      </div>
      {compare && <CompareDialog onClose={() => setCompare(false)} />}
    </>
  );
}

function OutputRow({ o, color }: { o: OutputInfo; color?: string | null }) {
  const evicted = o.state === "evicted";
  return (
    <div
      role="button"
      tabIndex={0}
      onDoubleClick={() => void openOutput(o)}
      onKeyDown={(e) => {
        if (e.key === "Enter") void openOutput(o);
        else if (e.key === "Delete" || (e.key === "Backspace" && (e.metaKey || e.ctrlKey))) {
          e.preventDefault();
          dropOutput(o);
        }
      }}
      className={`group mb-0.5 rounded-md px-2 py-1.5 hover:bg-hover ${evicted ? "opacity-60" : ""}`}
      title="Double-click to open"
    >
      <div className="flex items-center gap-1.5">
        <OutputChip output={o} compact />
        <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-muted">{sqlPreview(o.sql, 60)}</span>
        <button
          className="icon-btn hidden h-6 w-6 shrink-0 group-hover:flex group-focus-within:flex"
          title="Drop output (⌘⌫)"
          aria-label={`Drop output ${o.handle}`}
          onClick={(e) => {
            e.stopPropagation();
            dropOutput(o);
          }}
          onDoubleClick={(e) => e.stopPropagation()}
        >
          <Trash2 size={12} />
        </button>
      </div>
      <div className="mt-1 flex items-center gap-2 pl-0.5 text-[10.5px] text-muted">
        <span className="flex min-w-0 items-center gap-1 truncate">
          <ConnDot color={color} /> {o.connection_name}
        </span>
        <span>{formatCount(o.rows)} rows</span>
        <span>{formatAge(o.created_at)}</span>
        {o.pinned && <Pin size={10} className="text-accent" />}
        {o.state === "on_disk" && (
          <span title="Saved on disk; loads on first use">
            <HardDrive size={10} />
          </span>
        )}
        {o.truncated && (
          <span title="Capped at the row limit">
            <AlertTriangle size={10} className="text-warning" />
          </span>
        )}
        {evicted && <span>freed</span>}
      </div>
    </div>
  );
}
