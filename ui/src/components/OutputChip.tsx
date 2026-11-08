import { createContext, useContext, useEffect, useMemo, useState } from "react";
import { compareOptions, DEFAULT_COMPARE_FORM, KIND_HELP, KIND_LABEL, pairColumns, parseMapping, parseTolerance, setPair, shortType, type CompareForm } from "../lib/compare";
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
  Trash2,
} from "lucide-react";
import { api, toError } from "../lib/api";
import type { CompareKind, ComparePair, OutputInfo } from "../lib/types";
import { formatBytes, formatCount, sqlPreview } from "../lib/util";
import { useStore } from "../store";
import {
  copyOutputRef,
  dropOutput,
  formatAge,
  mentionInAi,
  openOutput,
  openResultsQuery,
  outputLabel,
  outputRef,
  outputQuerySql,
  queryOutput,
  renameOutput,
  togglePin,
} from "../outputs";
import { MenuItem, MenuSeparator, Modal, Popover } from "./ui";

/**
 * Where "Query with SQL" goes. Notebooks provide one that adds a cell below
 * the current cell; elsewhere a new query tab opens on the Results connection.
 */
export const OutputQueryTarget = createContext<
  ((sql: string, o: OutputInfo) => void) | null
>(null);

/** Badge for an output (`r12` / `revenue`) with its action menu. */
export function OutputChip({
  output,
  compact,
}: {
  output: OutputInfo;
  compact?: boolean;
}) {
  const live =
    useStore((s) => s.outputs.find((o) => o.handle === output.handle)) ??
    output;
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [compare, setCompare] = useState(false);
  const queryTarget = useContext(OutputQueryTarget);
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
            o.pinned
              ? "border-accent/50 bg-accent/12 text-accent"
              : "border-line bg-panel-2 text-fg hover:bg-hover"
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
        <Popover
          x={menu.x}
          y={menu.y}
          onClose={() => setMenu(null)}
          className="w-72"
        >
          <div className="border-b border-line px-2 pb-2 pt-1 text-[11.5px]">
            <div className="flex items-center gap-1.5 font-mono text-[12px]">
              {outputRef(o)}
              <span className="ml-auto rounded bg-panel-2 px-1 text-[10.5px] text-muted">
                {o.handle}
              </span>
            </div>
            <div className="mt-1 flex items-center gap-1 text-muted">
              <Database size={11} /> {o.connection_name} ·{" "}
              {formatAge(o.created_at)}
            </div>
            <div className="text-muted">
              {formatCount(o.rows)} rows × {o.columns.length} cols ·{" "}
              {formatBytes(o.bytes)}
              {o.state === "on_disk"
                ? " · on disk"
                : o.state === "evicted"
                  ? " · freed"
                  : ""}
            </div>
            {o.truncated && (
              <div className="mt-1 flex items-start gap-1 text-warning">
                <AlertTriangle size={11} className="mt-px shrink-0" /> Capped at{" "}
                {formatCount(o.rows)} rows — totals over it are incomplete.
              </div>
            )}
            <div className="mt-1 line-clamp-3 font-mono text-[10.5px] text-muted">
              {sqlPreview(o.sql, 220)}
            </div>
          </div>
          <MenuItem
            icon={<Copy size={13} />}
            label={`Copy ${outputRef(o)}`}
            onClick={() => (setMenu(null), void copyOutputRef(o))}
          />
          <MenuItem
            icon={<Pencil size={13} />}
            label={o.name ? "Rename…" : "Name this output…"}
            onClick={() => (setMenu(null), setRenaming(true))}
          />
          <MenuItem
            icon={o.pinned ? <PinOff size={13} /> : <Pin size={13} />}
            label={o.pinned ? "Unpin" : "Pin (keep across runs and restarts)"}
            disabled={o.state === "evicted"}
            onClick={() => (setMenu(null), void togglePin(o))}
          />
          <MenuSeparator />
          <MenuItem
            icon={<TerminalSquare size={13} />}
            label={
              queryTarget
                ? "Query with SQL in a new cell"
                : "Query with SQL (DuckDB)"
            }
            onClick={() => (
              setMenu(null),
              queryTarget ? queryTarget(outputQuerySql(o), o) : queryOutput(o)
            )}
          />
          <MenuItem
            icon={<GitCompare size={13} />}
            label="Compare with…"
            onClick={() => (setMenu(null), setCompare(true))}
          />
          <MenuItem
            icon={<ExternalLink size={13} />}
            label="Open in its own tab"
            disabled={o.state === "evicted"}
            onClick={() => (setMenu(null), void openOutput(o))}
          />
          <MenuItem
            icon={<AtSign size={13} />}
            label="Ask AI about it"
            onClick={() => (setMenu(null), mentionInAi(o))}
          />
          {o.state === "on_disk" && (
            <MenuItem
              icon={<HardDrive size={13} />}
              label="Load from disk"
              onClick={() => (setMenu(null), void api.loadOutput(o.handle))}
            />
          )}
          <MenuSeparator />
          <MenuItem
            icon={<Trash2 size={13} />}
            label="Drop output…"
            danger
            onClick={() => (setMenu(null), dropOutput(o))}
          />
        </Popover>
      )}
      {compare && (
        <CompareDialog initial={o} onClose={() => setCompare(false)} />
      )}
    </>
  );
}

/** Pick two outputs + key columns, open a diff query on the Results connection. */
export function CompareDialog({
  initial,
  onClose,
}: {
  initial?: OutputInfo;
  onClose: () => void;
}) {
  // Select the stored array and derive in a memo: a selector returning a new
  // array each call makes zustand re-render forever (blank app).
  const all = useStore((s) => s.outputs);
  const outputs = useMemo(
    () => all.filter((o) => o.state !== "evicted"),
    [all],
  );
  const toast = useStore((s) => s.toast);
  const defaultBefore = useMemo(() => {
    if (!initial) return outputs[1]?.handle ?? "";
    // Previous version of the same name, else the previous output of the same tab.
    const name = initial.name ?? initial.version_of?.[0];
    const prevVersion = outputs.find(
      (o) =>
        o.version_of && o.version_of[0] === name && o.handle !== initial.handle,
    );
    const sameTab = outputs.find(
      (o) =>
        o.tab_id === initial.tab_id &&
        o.handle !== initial.handle &&
        o.statement_index === initial.statement_index,
    );
    return (
      (
        prevVersion ??
        sameTab ??
        outputs.find((o) => o.handle !== initial.handle)
      )?.handle ?? ""
    );
  }, [initial, outputs]);
  const [before, setBefore] = useState(defaultBefore);
  const [after, setAfter] = useState(
    initial?.handle ?? outputs[0]?.handle ?? "",
  );
  const b = outputs.find((o) => o.handle === before);
  const a = outputs.find((o) => o.handle === after);
  // Columns matched by name (any case) plus pairs the user adds.
  const [manual, setManual] = useState<[string, string][]>([]);
  /** After-side columns left out of the comparison. */
  const [excluded, setExcluded] = useState<string[]>([]);
  const [typed, setTyped] = useState("");
  const [typedErrors, setTypedErrors] = useState<string[]>([]);
  const beforeCols = useMemo(() => b?.columns.map((c) => c.name) ?? [], [b]);
  const afterCols = useMemo(() => a?.columns.map((c) => c.name) ?? [], [a]);
  const match = useMemo(() => pairColumns(beforeCols, afterCols, manual, excluded), [beforeCols, afterCols, manual, excluded]);
  const choose = (after: string, before: string | null) => {
    const next = setPair(manual, excluded, after, before);
    setManual(next.manual);
    setExcluded(next.excluded);
  };
  const applyTyped = () => {
    const r = parseMapping(typed, beforeCols, afterCols);
    let st = { manual, excluded };
    for (const [bc, ac] of r.pairs) st = setPair(st.manual, st.excluded, ac, bc);
    setManual(st.manual);
    setExcluded(st.excluded);
    setTypedErrors(r.errors);
    if (!r.errors.length) setTyped("");
  };
  const common = useMemo(() => match.pairs.map((p) => p.after), [match]);
  useEffect(() => {
    setManual([]);
    setExcluded([]);
    setTyped("");
    setTypedErrors([]);
    setForm((f) => ({ ...f, kinds: {} }));
  }, [before, after]);
  // How values compare: options for all columns, a kind per column.
  const [form, setForm] = useState<CompareForm>(DEFAULT_COMPARE_FORM);
  const tolerance = parseTolerance(form.tolerance);
  const toleranceOk = !Number.isNaN(tolerance);
  const options = useMemo(() => compareOptions(form, common), [form, common]);
  /** Types and detected kinds of the matched pairs (from the backend). */
  const [info, setInfo] = useState<ComparePair[]>([]);
  useEffect(() => {
    if (!a || !b || match.pairs.length === 0) return setInfo([]);
    let alive = true;
    api
      .outputComparePairs(b.handle, a.handle, match.pairs.map((p) => [p.before, p.after] as [string, string]), true)
      .then((p) => alive && setInfo(p))
      .catch(() => alive && setInfo([]));
    return () => {
      alive = false;
    };
  }, [a, b, match]);
  const setKind = (col: string, kind: CompareKind | "") =>
    setForm((f) => {
      const kinds = { ...f.kinds };
      if (kind) kinds[col] = kind;
      else delete kinds[col];
      return { ...f, kinds };
    });
  const [keys, setKeys] = useState<string[]>(() => {
    const first = initial?.columns[0]?.name;
    return first && /(^id$|_id$|^key$|^code$|name$|country|date)/i.test(first)
      ? [first]
      : [];
  });

  const run = async () => {
    if (!a || !b) return;
    try {
      const sql = await api.outputDiffSql(
        b.handle,
        a.handle,
        keys.filter((k) => common.includes(k)),
        // Every pair as shown (the user may have left some out).
        match.pairs.map((p) => [p.before, p.after] as [string, string]),
        true,
        options,
      );
      await openResultsQuery(sql, `Diff ${outputLabel(b)} → ${outputLabel(a)}`);
      onClose();
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };

  const pick = (value: string, set: (v: string) => void, label: string) => (
    <label className="block">
      <span className="mb-1 block text-[11.5px] font-medium text-muted">
        {label}
      </span>
      <select
        className="field"
        value={value}
        onChange={(e) => set(e.target.value)}
      >
        {outputs.map((o) => (
          <option key={o.handle} value={o.handle}>
            {outputLabel(o)} ({o.handle}) — {o.connection_name},{" "}
            {formatCount(o.rows)} rows, {formatAge(o.created_at)}
          </option>
        ))}
      </select>
    </label>
  );

  return (
    <Modal
      title="Compare outputs"
      onClose={onClose}
      width={680}
      footer={
        <>
          <button className="btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn-primary"
            disabled={!a || !b || a.handle === b.handle || common.length === 0 || !toleranceOk}
            onClick={() => void run()}
          >
            <GitCompare size={14} /> Show differences
          </button>
        </>
      }
    >
      <div className="space-y-3 text-[12.5px]">
        {pick(before, setBefore, "Before")}
        {pick(after, setAfter, "After")}
        <div>
          <div className="mb-1 text-[11.5px] font-medium text-muted">
            Key columns (match rows by)
          </div>
          {common.length === 0 ? (
            <p className="text-danger">
              No columns match by name. Match columns below to compare them.
            </p>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {common.map((c) => (
                <label
                  key={c}
                  className={`flex cursor-pointer items-center gap-1 rounded-md border px-2 py-0.5 font-mono text-[11.5px] ${keys.includes(c) ? "border-accent bg-accent/10" : "border-line"}`}
                >
                  <input
                    type="checkbox"
                    className="hidden"
                    checked={keys.includes(c)}
                    onChange={(e) =>
                      setKeys(
                        e.target.checked
                          ? [...keys, c]
                          : keys.filter((k) => k !== c),
                      )
                    }
                  />
                  {c}
                </label>
              ))}
            </div>
          )}
          <p className="mt-1.5 text-[11.5px] text-muted">
            With keys: rows added, removed and changed (with before values).
            Without keys: whole rows that differ. Runs locally in DuckDB.
          </p>
        </div>

        <div>
          <div className="mb-1 text-[11.5px] font-medium text-muted">Values are equal when they mean the same</div>
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5 text-[12px]">
            <label className="flex items-center gap-1.5" title="CHAR padding and trailing spaces: 'ABC  ' = 'ABC'">
              <input type="checkbox" checked={form.trim} onChange={(e) => setForm({ ...form, trim: e.target.checked })} /> Ignore spaces at the ends
            </label>
            <label className="flex items-center gap-1.5" title="'ABC' = 'abc'">
              <input type="checkbox" checked={form.ignoreCase} onChange={(e) => setForm({ ...form, ignoreCase: e.target.checked })} /> Ignore case
            </label>
            <label className="flex items-center gap-1.5" title="An empty text equals NULL">
              <input type="checkbox" checked={form.emptyIsNull} onChange={(e) => setForm({ ...form, emptyIsNull: e.target.checked })} /> Empty text = NULL
            </label>
            <label className="flex items-center gap-1.5" title={keys.length ? "Numbers this close are equal" : "Without keys, numbers are rounded to steps of this"}>
              Number tolerance
              <input
                className={`field h-6 w-20 py-0 text-[11.5px] ${toleranceOk ? "" : "border-danger"}`}
                inputMode="decimal"
                placeholder="exact"
                aria-label="Number tolerance"
                value={form.tolerance}
                onChange={(e) => setForm({ ...form, tolerance: e.target.value })}
              />
            </label>
          </div>
          {!toleranceOk && <p className="mt-1 text-[11.5px] text-danger">The tolerance must be a positive number, e.g. 0.01.</p>}
          <p className="mt-1 text-[11.5px] text-muted">
            Dates, numbers and booleans compare by value whatever type each database gave them (2026-11-11 00:00:00 = '2026-11-11', 10.50 = '10.5'). Change it per column below.
          </p>
        </div>

        <div>
          <div className="mb-1 flex items-center text-[11.5px] font-medium text-muted">
            <span>
              Matched columns ({match.pairs.length} of {afterCols.length})
            </span>
            {(manual.length > 0 || excluded.length > 0) && (
              <button
                className="ml-auto text-[11px] font-normal text-accent hover:underline"
                onClick={() => (setManual([]), setExcluded([]))}
              >
                Reset to automatic
              </button>
            )}
          </div>
          <div className="max-h-56 overflow-auto rounded-md border border-line">
            <table className="w-full font-mono text-[11.5px]">
              <thead className="sticky top-0 bg-panel-2 font-sans text-[10.5px] text-muted">
                <tr>
                  <th className="px-2 py-1 text-left font-medium">After ({a ? outputLabel(a) : "…"})</th>
                  <th className="w-4" />
                  <th className="px-2 py-1 text-left font-medium">Before ({b ? outputLabel(b) : "…"})</th>
                  <th className="px-2 py-1 text-left font-medium">Compare as</th>
                  <th className="px-2 py-1 text-right font-medium">How</th>
                </tr>
              </thead>
              <tbody>
                {afterCols.map((ac) => {
                  const p = match.pairs.find((x) => x.after === ac);
                  const pi = p ? info.find((x) => x.after === ac && x.before === p.before) : undefined;
                  const types = pi && shortType(pi.before_type) !== shortType(pi.after_type) ? `${shortType(pi.before_type)} → ${shortType(pi.after_type)}` : null;
                  const chosen = form.kinds[ac];
                  return (
                    <tr key={ac} className="border-t border-line/60">
                      <td className="max-w-0 truncate px-2 py-0.5" title={ac}>
                        {ac}
                      </td>
                      <td className="text-center text-muted">←</td>
                      <td className="px-1 py-0.5">
                        <select
                          className={`field h-6 w-full py-0 font-mono text-[11.5px] ${p ? "" : "text-muted"}`}
                          aria-label={`Before column matched to ${ac}`}
                          value={p?.before ?? ""}
                          onChange={(e) => choose(ac, e.target.value || null)}
                        >
                          <option value="">— not compared —</option>
                          {beforeCols.map((bc) => {
                            const other = match.pairs.find((x) => x.before === bc && x.after !== ac);
                            return (
                              <option key={bc} value={bc}>
                                {bc}
                                {other ? `  (now ↔ ${other.after})` : ""}
                              </option>
                            );
                          })}
                        </select>
                      </td>
                      <td className="px-1 py-0.5 font-sans">
                        {p && pi && (
                          <>
                            <select
                              className="field h-6 w-full py-0 text-[11.5px]"
                              aria-label={`Compare ${ac} as`}
                              title={KIND_HELP[chosen ?? pi.auto_kind]}
                              value={chosen ?? ""}
                              onChange={(e) => setKind(ac, e.target.value as CompareKind | "")}
                            >
                              <option value="">Auto · {KIND_LABEL[pi.auto_kind]}</option>
                              {(Object.keys(KIND_LABEL) as CompareKind[]).map((k) => (
                                <option key={k} value={k}>
                                  {KIND_LABEL[k]}
                                </option>
                              ))}
                            </select>
                            {types && (
                              <div className="truncate text-[10.5px] text-muted" title={`${pi.before_type} → ${pi.after_type}`}>
                                {types}
                              </div>
                            )}
                          </>
                        )}
                      </td>
                      <td className="whitespace-nowrap px-2 py-0.5 text-right font-sans text-[10.5px] text-muted">
                        {!p ? "" : p.how === "same" ? "same name" : p.how === "case" ? "other case" : "by you"}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          {match.onlyBefore.length > 0 && (
            <div className="mt-1 text-[11.5px] text-muted">
              Before columns not compared: <span className="font-mono">{match.onlyBefore.join(", ")}</span>
            </div>
          )}
          <div className="mt-2 flex items-center gap-1.5">
            <input
              className="field h-7 min-w-0 flex-1 py-0 font-mono text-[11.5px]"
              placeholder="Match several at once: a = a1, b = b2"
              aria-label="Column pairs (before = after)"
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && typed.trim() && applyTyped()}
            />
            <button className="btn-ghost shrink-0 border border-line py-1" disabled={!typed.trim()} onClick={applyTyped}>
              Match
            </button>
          </div>
          {typedErrors.length > 0 && (
            <ul className="mt-1 space-y-0.5 text-[11.5px] text-danger">
              {typedErrors.map((e) => (
                <li key={e}>{e}</li>
              ))}
            </ul>
          )}
          {(a?.truncated || b?.truncated) && (
            <p className="mt-1 flex items-center gap-1 text-[11.5px] text-warning">
              <AlertTriangle size={11} /> One side is capped at the row limit,
              so missing rows may be reported as removed or added.
            </p>
          )}
        </div>
      </div>
    </Modal>
  );
}
