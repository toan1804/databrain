import { useMemo, useState } from "react";
import { Search } from "lucide-react";
import { outputLabel, outputRef } from "../../outputs";
import { formatCount, sqlPreview } from "../../lib/util";
import type { OutputInfo } from "../../lib/types";
import { useStore } from "../../store";
import { Popover } from "../ui";

/**
 * Results that can be added to tab `tabId`'s result tabs: other tabs'
 * outputs and named ones (`results.<name>`) that still have their rows, not
 * the tab's own and not those already added, most recently used first.
 */
export function addableOutputs(outputs: OutputInfo[], tabId: string, shown: Set<string>): OutputInfo[] {
  return outputs
    .filter((o) => o.state !== "evicted" && o.tab_id !== tabId && !shown.has(o.handle))
    .sort((a, b) => b.last_used - a.last_used);
}

/** Pick a result to add to this tab's result tabs (read-only there). */
export function AddResultPicker({ tabId, x, y, shown, onClose }: { tabId: string; x: number; y: number; shown: Set<string>; onClose: () => void }) {
  const outputs = useStore((s) => s.outputs);
  const addResult = useStore((s) => s.addResult);
  const [q, setQ] = useState("");
  const list = useMemo(() => {
    const words = q.toLowerCase().split(/\s+/).filter(Boolean);
    return addableOutputs(outputs, tabId, shown).filter((o) => {
      const hay = `${outputRef(o)} ${o.handle} ${o.connection_name} ${o.sql}`.toLowerCase();
      return words.every((w) => hay.includes(w));
    });
  }, [outputs, tabId, shown, q]);
  const pick = (o: OutputInfo) => {
    onClose();
    void addResult(tabId, o);
  };
  return (
    <Popover x={x} y={y} onClose={onClose} className="w-[420px]">
      <div role="dialog" aria-label="Add a result">
        <div className="flex items-center gap-1.5 border-b border-line px-2 pb-1.5 pt-1">
          <Search size={12} className="shrink-0 text-muted" />
          <input
            autoFocus
            className="min-w-0 flex-1 bg-transparent text-[12.5px] outline-none"
            placeholder="Find a result by name, connection or SQL…"
            aria-label="Find a result to add"
            value={q}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && list[0]) pick(list[0]);
            }}
          />
        </div>
        <div className="max-h-[320px] overflow-y-auto py-1" role="listbox" aria-label="Results to add">
          {list.length === 0 ? (
            <div className="px-2 py-3 text-[12px] text-muted">
              {q ? "No result matches." : "No other results yet: run a query in another tab, or name one (results.<name>)."}
            </div>
          ) : (
            list.map((o) => (
              <button
                key={o.handle}
                role="option"
                aria-selected={false}
                className="flex w-full flex-col items-start gap-0.5 rounded-md px-2 py-1.5 text-left hover:bg-hover"
                title={o.sql}
                onClick={() => pick(o)}
              >
                <span className="flex w-full items-center gap-2 text-[12.5px]">
                  <span className="truncate font-mono">{outputLabel(o)}</span>
                  <span className="ml-auto shrink-0 text-[11px] text-muted">
                    {o.connection_name} · {formatCount(o.rows)} rows
                  </span>
                </span>
                <span className="w-full truncate font-mono text-[11px] text-muted">{sqlPreview(o.sql, 90)}</span>
              </button>
            ))
          )}
        </div>
        <div className="border-t border-line px-2 pt-1.5 text-[11px] text-muted">Shown read-only in this tab until you close it; it stays after a restart.</div>
      </div>
    </Popover>
  );
}
