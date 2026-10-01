import { useMemo, useState } from "react";
import {
  AlertTriangle,
  AtSign,
  Copy,
  Database,
  ExternalLink,
  GitCompare,
  HardDrive,
  Pencil,
  Pin,
  PinOff,
  TerminalSquare,
} from "lucide-react";
import { api, toError } from "../lib/api";
import type { OutputInfo } from "../lib/types";
import { formatBytes, formatCount, sqlPreview } from "../lib/util";
import { useStore } from "../store";
import {
  copyOutputRef,
  formatAge,
  mentionInAi,
  openOutput,
  openResultsQuery,
  outputLabel,
  outputRef,
  queryOutput,
  renameOutput,
  togglePin,
} from "../outputs";
import { MenuItem, MenuSeparator, Modal, Popover } from "./ui";

/** Badge for an output (`r12` / `revenue`) with its action menu. */
export function OutputChip({ output, compact }: { output: OutputInfo; compact?: boolean }) {
  const live = useStore((s) => s.outputs.find((o) => o.handle === output.handle)) ?? output;
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [compare, setCompare] = useState(false);
  const o = live;
  const label = outputLabel(o);
  return (
    <>
      {renaming ? (
        <input
          autoFocus
          className="field h-6 w-36 py-0 font-mono text-[11.5px]"
          defaultValue={o.name ?? ""}
          placeholder="name, e.g. revenue"
          aria-label="Output name"
          onBlur={(e) => {
            setRenaming(false);
            const v = e.target.value.trim();
            if (v !== (o.name ?? "")) void renameOutput(o, v || null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
            if (e.key === "Escape") setRenaming(false);
          }}
        />
      ) : (
        <button
          className={`flex h-6 shrink-0 items-center gap-1 rounded-md border px-1.5 font-mono text-[11.5px] ${
            o.pinned ? "border-accent/50 bg-accent/12 text-accent" : "border-line bg-panel-2 text-fg hover:bg-hover"
          }`}
          title={`${outputRef(o)} — click for actions${o.truncated ? "\nOnly the first rows are stored (row limit)" : ""}`}
          aria-label={`Output ${label}`}
          onClick={(e) => setMenu({ x: e.clientX, y: e.clientY + 8 })}
          onDoubleClick={() => setRenaming(true)}
        >
          {o.pinned ? <Pin size={10} /> : <span className="text-muted">#</span>}
          {label}
          {!compact && o.name && <span className="text-muted">{o.handle}</span>}
          {o.truncated && <AlertTriangle size={10} className="text-warning" />}
        </button>
      )}
      {menu && (
        <Popover x={menu.x} y={menu.y} onClose={() => setMenu(null)} className="w-72">
          <div className="border-b border-line px-2 pb-2 pt-1 text-[11.5px]">
            <div className="flex items-center gap-1.5 font-mono text-[12px]">
              {outputRef(o)}
              <span className="ml-auto rounded bg-panel-2 px-1 text-[10.5px] text-muted">{o.handle}</span>
            </div>
            <div className="mt-1 flex items-center gap-1 text-muted">
              <Database size={11} /> {o.connection_name} · {formatAge(o.created_at)}
            </div>
            <div className="text-muted">
              {formatCount(o.rows)} rows × {o.columns.length} cols · {formatBytes(o.bytes)}
              {o.state === "on_disk" ? " · on disk" : o.state === "evicted" ? " · freed" : ""}
            </div>
            {o.truncated && (
              <div className="mt-1 flex items-start gap-1 text-warning">
                <AlertTriangle size={11} className="mt-px shrink-0" /> Capped at {formatCount(o.rows)} rows — totals over it are incomplete.
              </div>
            )}
            <div className="mt-1 line-clamp-3 font-mono text-[10.5px] text-muted">{sqlPreview(o.sql, 220)}</div>
          </div>
          <MenuItem icon={<Copy size={13} />} label={`Copy ${outputRef(o)}`} onClick={() => (setMenu(null), void copyOutputRef(o))} />
          <MenuItem icon={<Pencil size={13} />} label={o.name ? "Rename…" : "Name this output…"} onClick={() => (setMenu(null), setRenaming(true))} />
          <MenuItem
            icon={o.pinned ? <PinOff size={13} /> : <Pin size={13} />}
            label={o.pinned ? "Unpin" : "Pin (keep across runs and restarts)"}
            disabled={o.state === "evicted"}
            onClick={() => (setMenu(null), void togglePin(o))}
          />
          <MenuSeparator />
          <MenuItem icon={<TerminalSquare size={13} />} label="Query with SQL (DuckDB)" onClick={() => (setMenu(null), queryOutput(o))} />
          <MenuItem icon={<GitCompare size={13} />} label="Compare with…" onClick={() => (setMenu(null), setCompare(true))} />
          <MenuItem icon={<ExternalLink size={13} />} label="Open in its own tab" disabled={o.state === "evicted"} onClick={() => (setMenu(null), void openOutput(o))} />
          <MenuItem icon={<AtSign size={13} />} label="Ask AI about it" onClick={() => (setMenu(null), mentionInAi(o))} />
          {o.state === "on_disk" && <MenuItem icon={<HardDrive size={13} />} label="Load from disk" onClick={() => (setMenu(null), void api.loadOutput(o.handle))} />}
        </Popover>
      )}
      {compare && <CompareDialog initial={o} onClose={() => setCompare(false)} />}
    </>
  );
}

/** Pick two outputs + key columns, open a diff query on the Results connection. */
export function CompareDialog({ initial, onClose }: { initial?: OutputInfo; onClose: () => void }) {
  // Select the stored array and derive in a memo: a selector returning a new
  // array each call makes zustand re-render forever (blank app).
  const all = useStore((s) => s.outputs);
  const outputs = useMemo(() => all.filter((o) => o.state !== "evicted"), [all]);
  const toast = useStore((s) => s.toast);
  const defaultBefore = useMemo(() => {
    if (!initial) return outputs[1]?.handle ?? "";
    // Previous version of the same name, else the previous output of the same tab.
    const name = initial.name ?? initial.version_of?.[0];
    const prevVersion = outputs.find((o) => o.version_of && o.version_of[0] === name && o.handle !== initial.handle);
    const sameTab = outputs.find((o) => o.tab_id === initial.tab_id && o.handle !== initial.handle && o.statement_index === initial.statement_index);
    return (prevVersion ?? sameTab ?? outputs.find((o) => o.handle !== initial.handle))?.handle ?? "";
  }, [initial, outputs]);
  const [before, setBefore] = useState(defaultBefore);
  const [after, setAfter] = useState(initial?.handle ?? outputs[0]?.handle ?? "");
  const b = outputs.find((o) => o.handle === before);
  const a = outputs.find((o) => o.handle === after);
  const common = useMemo(() => (a && b ? a.columns.map((c) => c.name).filter((n) => b.columns.some((x) => x.name === n)) : []), [a, b]);
  const [keys, setKeys] = useState<string[]>(() => {
    const first = initial?.columns[0]?.name;
    return first && /(^id$|_id$|^key$|^code$|name$|country|date)/i.test(first) ? [first] : [];
  });

  const run = async () => {
    if (!a || !b) return;
    try {
      const sql = await api.outputDiffSql(b.handle, a.handle, keys.filter((k) => common.includes(k)));
      await openResultsQuery(sql, `Diff ${outputLabel(b)} → ${outputLabel(a)}`);
      onClose();
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };

  const pick = (value: string, set: (v: string) => void, label: string) => (
    <label className="block">
      <span className="mb-1 block text-[11.5px] font-medium text-muted">{label}</span>
      <select className="field" value={value} onChange={(e) => set(e.target.value)}>
        {outputs.map((o) => (
          <option key={o.handle} value={o.handle}>
            {outputLabel(o)} ({o.handle}) — {o.connection_name}, {formatCount(o.rows)} rows, {formatAge(o.created_at)}
          </option>
        ))}
      </select>
    </label>
  );

  return (
    <Modal
      title="Compare outputs"
      onClose={onClose}
      width={560}
      footer={
        <>
          <button className="btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button className="btn-primary" disabled={!a || !b || a.handle === b.handle || common.length === 0} onClick={() => void run()}>
            <GitCompare size={14} /> Show differences
          </button>
        </>
      }
    >
      <div className="space-y-3 text-[12.5px]">
        {pick(before, setBefore, "Before")}
        {pick(after, setAfter, "After")}
        <div>
          <div className="mb-1 text-[11.5px] font-medium text-muted">Key columns (match rows by)</div>
          {common.length === 0 ? (
            <p className="text-danger">These outputs have no columns in common.</p>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {common.map((c) => (
                <label key={c} className={`flex cursor-pointer items-center gap-1 rounded-md border px-2 py-0.5 font-mono text-[11.5px] ${keys.includes(c) ? "border-accent bg-accent/10" : "border-line"}`}>
                  <input type="checkbox" className="hidden" checked={keys.includes(c)} onChange={(e) => setKeys(e.target.checked ? [...keys, c] : keys.filter((k) => k !== c))} />
                  {c}
                </label>
              ))}
            </div>
          )}
          <p className="mt-1.5 text-[11.5px] text-muted">
            With keys: rows added, removed and changed (with before values). Without keys: whole rows that differ. Runs locally in DuckDB.
          </p>
          {(a?.truncated || b?.truncated) && (
            <p className="mt-1 flex items-center gap-1 text-[11.5px] text-warning">
              <AlertTriangle size={11} /> One side is capped at the row limit, so missing rows may be reported as removed or added.
            </p>
          )}
        </div>
      </div>
    </Modal>
  );
}
