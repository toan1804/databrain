// Compare of two outputs: counts at the top (each one a filter), changed
// rows per column, and the differing rows with changed cells shown as
// before → after. Counts are exact (a summary query); rows are paged from
// the diff result, filtered on the backend.
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  ArrowLeftRight,
  FileCode2,
  Loader2,
  RefreshCw,
  Settings2,
  Table2,
} from "lucide-react";
import { api, toError } from "../lib/api";
import {
  changedIn,
  diffLayout,
  diffView,
  KIND_LABEL,
  shortType,
  type DiffFilter,
} from "../lib/compare";
import type { CompareRun, CompareSpec } from "../lib/types";
import { formatCount } from "../lib/util";
import { openOutput, openResultsQuery, outputLabel } from "../outputs";
import { useStore } from "../store";
import { CompareDialog } from "./OutputChip";
import { sourceLabel } from "./CompareSide";

/** Rows fetched at first and per "Show more". */
const PAGE = 200;

const FILTERS: {
  key: "changed" | "added" | "removed";
  label: string;
  cls: string;
}[] = [
  { key: "changed", label: "Changed", cls: "text-warning" },
  { key: "removed", label: "Only before", cls: "text-danger" },
  { key: "added", label: "Only after", cls: "text-success" },
];

export function OutputCompareTab({
  tabId,
  visible,
}: {
  tabId: string;
  visible: boolean;
}) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const updateTab = useStore((s) => s.updateTab);
  const toast = useStore((s) => s.toast);
  const outputs = useStore((s) => s.outputs);
  const connections = useStore((s) => s.connections);
  const connName = (id: string) => connections.find((c) => c.id === id)?.name;
  const spec = tab?.diff?.spec;
  const [run, setRun] = useState<CompareRun | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState<DiffFilter>("all");
  const [column, setColumn] = useState<string | null>(null);
  const [onlyDiffering, setOnlyDiffering] = useState(true);
  const [rows, setRows] = useState<(string | null)[][]>([]);
  const [viewRows, setViewRows] = useState(0);
  const [editing, setEditing] = useState(false);

  const sources = tab?.diff?.sources;
  const compare = useCallback(async () => {
    if (!spec) return;
    setBusy(true);
    setError(null);
    try {
      setRun(await api.compareOutputs(spec, tabId));
    } catch (e) {
      setRun(null);
      setError(toError(e).message);
    } finally {
      setBusy(false);
    }
  }, [spec, tabId]);
  useEffect(() => {
    void compare();
  }, [compare]);

  const compared = useMemo(() => run?.pairs.map((p) => p.after) ?? [], [run]);
  const layout = useMemo(
    () =>
      run
        ? diffLayout(
            run.result.columns.map((c) => c.name),
            compared,
          )
        : null,
    [run, compared],
  );
  const values = useMemo(
    () => compared.filter((c) => !run?.keys.includes(c)),
    [compared, run],
  );
  const changedCount = useMemo(
    () => new Map(run?.summary.columns.map((c) => [c.column, c.changed]) ?? []),
    [run],
  );
  /** Columns shown: keys, then compared columns (with changes only, when asked and keyed). */
  const shownValues = useMemo(
    () =>
      onlyDiffering && run?.keys.length
        ? values.filter((c) => (changedCount.get(c) ?? 0) > 0)
        : values,
    [onlyDiffering, run, values, changedCount],
  );

  // First page whenever the compare, the filter or the column changes.
  useEffect(() => {
    if (!run || !layout) return;
    let alive = true;
    api
      .fetchPage(run.result.id, diffView(layout, filter, column), 0, PAGE)
      .then(
        (p) => {
          if (!alive) return;
          setRows(p.rows);
          setViewRows(p.view_rows);
        },
        (e) => alive && toast(toError(e).message, "error"),
      );
    return () => {
      alive = false;
    };
  }, [run, layout, filter, column, toast]);
  const more = () => {
    if (!run || !layout) return;
    api
      .fetchPage(
        run.result.id,
        diffView(layout, filter, column),
        rows.length,
        PAGE,
      )
      .then((p) => setRows((r) => [...r, ...p.rows]))
      .catch((e) => toast(toError(e).message, "error"));
  };

  if (!tab || !spec) return null;
  const s = run?.summary;
  /** Compare again: tables and files are read again first. */
  const again = async () => {
    let next = spec;
    setBusy(true);
    try {
      for (const side of ["before", "after"] as const) {
        const src = sources?.[side];
        if (!src || src.kind === "output") continue;
        const o = await api.compareLoadSource(src, tabId, side);
        next = { ...next, [side]: o.handle };
      }
    } catch (e) {
      setBusy(false);
      return setError(toError(e).message);
    }
    setBusy(false);
    if (next === spec) void compare();
    else updateTab(tab.id, { diff: { spec: next, sources } });
  };
  const keyed = !!run?.keys.length;
  const setSpec = (next: CompareSpec, nextSources = sources) => {
    updateTab(tab.id, { diff: { spec: next, sources: nextSources } });
    setFilter("all");
    setColumn(null);
  };
  const swap = () =>
    setSpec(
      {
        ...spec,
        before: spec.after,
        after: spec.before,
        mapping: spec.mapping.map(([b, a]) => [a, b] as [string, string]),
      },
      { before: sources?.after ?? null, after: sources?.before ?? null },
    );
  const resultOutput = run
    ? outputs.find((o) => o.result_id === run.result.id)
    : undefined;
  const pick = (f: DiffFilter, c: string | null = null) => {
    setFilter(f);
    setColumn(c);
  };

  return (
    <div
      className="flex h-full min-h-0 flex-col text-[12.5px]"
      style={{ display: visible ? "flex" : "none" }}
    >
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line px-3 py-2">
        <Side
          label="Before"
          tone="danger"
          o={run?.before}
          fallback={spec.before}
          rows={s?.before_rows}
          from={sourceLabel(sources?.before, connName)}
        />
        <button
          className="icon-btn"
          title="Swap before and after"
          aria-label="Swap before and after"
          onClick={swap}
        >
          <ArrowLeftRight size={14} />
        </button>
        <Side
          label="After"
          tone="success"
          o={run?.after}
          fallback={spec.after}
          rows={s?.after_rows}
          from={sourceLabel(sources?.after, connName)}
        />
        <div className="ml-auto flex flex-wrap gap-1.5">
          <button
            className="btn-ghost border border-line py-1"
            onClick={() => setEditing(true)}
            title="Keys, matched columns and how values compare"
          >
            <Settings2 size={13} /> Keys & columns
          </button>
          <button
            className="btn-ghost border border-line py-1"
            disabled={busy}
            onClick={() => void again()}
            title={
              sources?.before || sources?.after
                ? "Read the tables and files again, then compare"
                : "Compare the outputs again"
            }
          >
            {busy ? (
              <Loader2 size={13} className="animate-spin" />
            ) : (
              <RefreshCw size={13} />
            )}{" "}
            Compare again
          </button>
          {run && (
            <button
              className="btn-ghost border border-line py-1"
              onClick={() =>
                void openResultsQuery(
                  run.sql,
                  `Diff ${outputLabel(run.before)} → ${outputLabel(run.after)}`,
                  false,
                )
              }
              title="The DuckDB SQL of this compare, in a query tab"
            >
              <FileCode2 size={13} /> Open SQL
            </button>
          )}
          {resultOutput && (
            <button
              className="btn-ghost border border-line py-1"
              onClick={() => void openOutput(resultOutput)}
              title="The differences as an output: export, chart, name or pin it"
            >
              <Table2 size={13} /> As output
            </button>
          )}
        </div>
      </div>

      {error && (
        <p className="p-3 text-danger" role="alert">
          {error}
        </p>
      )}
      {!run && !error && (
        <p className="p-4 text-muted">{busy ? "Comparing…" : ""}</p>
      )}

      {run && s && layout && (
        <>
          <div
            className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line bg-panel-2 px-3 py-2"
            aria-live="polite"
          >
            <Count
              label={keyed ? "Same" : "Same rows"}
              n={s.same}
              cls="text-muted"
            />
            {FILTERS.filter((f) => keyed || f.key !== "changed").map((f) => (
              <Count
                key={f.key}
                label={f.label}
                n={s[f.key]}
                cls={f.cls}
                active={filter === f.key && !column}
                onClick={() =>
                  pick(filter === f.key && !column ? "all" : f.key)
                }
              />
            ))}
            {(s.before_duplicate_keys > 0 || s.after_duplicate_keys > 0) && (
              <span
                className="flex items-center gap-1 text-[11.5px] text-warning"
                title="A key that isn't unique matches several rows, so changes for it are unreliable. Add key columns until each key is unique."
              >
                <AlertTriangle size={12} /> Duplicate keys:{" "}
                {formatCount(s.before_duplicate_keys)} before,{" "}
                {formatCount(s.after_duplicate_keys)} after
              </span>
            )}
            {!keyed && (
              <span className="text-[11.5px] text-muted">
                No keys: whole rows are compared. Choose key columns to see what
                changed in a row.
              </span>
            )}
          </div>

          {keyed && s.changed > 0 && (
            <div className="flex shrink-0 flex-wrap items-center gap-1.5 border-b border-line px-3 py-1.5 text-[11.5px]">
              <span className="text-muted">Changed by column:</span>
              {s.columns
                .filter((c) => c.changed > 0)
                .map((c) => (
                  <button
                    key={c.column}
                    className={`rounded border px-1.5 py-0.5 font-mono ${column === c.column ? "border-accent bg-accent/10" : "border-line hover:bg-hover"}`}
                    onClick={() =>
                      column === c.column
                        ? pick("all")
                        : pick("changed", c.column)
                    }
                    title={`Rows where ${c.column} changed`}
                  >
                    {c.column}{" "}
                    <span className="text-warning">
                      {formatCount(c.changed)}
                    </span>
                  </button>
                ))}
              <label className="ml-auto flex items-center gap-1 text-muted">
                <input
                  type="checkbox"
                  checked={onlyDiffering}
                  onChange={(e) => setOnlyDiffering(e.target.checked)}
                />{" "}
                Only columns that differ
              </label>
            </div>
          )}

          <div className="min-h-0 flex-1 overflow-auto">
            {viewRows === 0 ? (
              <p className="p-4 text-muted">
                {s.added + s.removed + s.changed === 0
                  ? "No differences: the outputs hold the same data."
                  : "No rows match this filter."}
              </p>
            ) : (
              <table className="w-max min-w-full border-separate border-spacing-0 text-[12px]">
                <thead className="sticky top-0 z-10 bg-panel">
                  <tr>
                    <th className="border-b border-line px-2 py-1 text-left font-medium text-muted">
                      Change
                    </th>
                    {[...run.keys, ...(keyed ? shownValues : values)].map(
                      (c) => {
                        const p = run.pairs.find((x) => x.after === c);
                        return (
                          <th
                            key={c}
                            className="border-b border-line px-2 py-1 text-left font-medium"
                            title={
                              p
                                ? `${p.before} → ${p.after} · ${shortType(p.before_type)} → ${shortType(p.after_type)} · compared as ${KIND_LABEL[p.kind]}`
                                : c
                            }
                          >
                            <span className="font-mono">{c}</span>
                            {run.keys.includes(c) && (
                              <span className="ml-1 text-[10px] text-accent">
                                key
                              </span>
                            )}
                          </th>
                        );
                      },
                    )}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r, i) => {
                    const change = r[layout.change] ?? "";
                    const diffs = changedIn(
                      layout.changed >= 0 ? r[layout.changed] : null,
                      values,
                    );
                    const tint =
                      change === "added"
                        ? "bg-success/5"
                        : change === "removed"
                          ? "bg-danger/5"
                          : "";
                    return (
                      <tr key={i} className={tint}>
                        <td
                          className={`border-b border-line/50 px-2 py-0.5 ${change === "added" ? "text-success" : change === "removed" ? "text-danger" : "text-warning"}`}
                        >
                          {change === "added"
                            ? "only after"
                            : change === "removed"
                              ? "only before"
                              : "changed"}
                        </td>
                        {run.keys.map((k) => (
                          <td
                            key={k}
                            className="border-b border-line/50 px-2 py-0.5 font-mono"
                          >
                            <Cell v={r[layout.value[k]]} />
                          </td>
                        ))}
                        {(keyed ? shownValues : values).map((c) => {
                          const after = r[layout.value[c]];
                          if (change !== "changed") {
                            return (
                              <td
                                key={c}
                                className="border-b border-line/50 px-2 py-0.5 font-mono"
                              >
                                <Cell v={after} />
                              </td>
                            );
                          }
                          const differs = diffs.has(c);
                          return (
                            <td
                              key={c}
                              className={`border-b border-line/50 px-2 py-0.5 font-mono ${differs ? "bg-warning/10" : "text-muted/70"}`}
                            >
                              {differs ? (
                                <span>
                                  <span className="rounded-sm bg-danger/10 px-0.5 text-danger line-through decoration-danger/50">
                                    <Cell v={r[layout.before[c]]} />
                                  </span>
                                  <span className="px-1 text-muted">→</span>
                                  <span className="rounded-sm bg-success/10 px-0.5 font-medium text-success">
                                    <Cell v={after} />
                                  </span>
                                </span>
                              ) : (
                                <Cell v={after} />
                              )}
                            </td>
                          );
                        })}
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            )}
            {rows.length < viewRows && (
              <div className="p-2 text-center">
                <button
                  className="btn-ghost py-0.5 text-[11.5px]"
                  onClick={more}
                >
                  Show {formatCount(Math.min(PAGE, viewRows - rows.length))}{" "}
                  more of {formatCount(viewRows - rows.length)}
                </button>
              </div>
            )}
            {run.result.truncated && (
              <p className="flex items-center gap-1 px-3 py-2 text-[11.5px] text-warning">
                <AlertTriangle size={12} /> Only the first{" "}
                {formatCount(run.result.total_rows)} differing rows are listed;
                the counts above are complete.
              </p>
            )}
            {(run.before.truncated || run.after.truncated) && (
              <p className="flex items-center gap-1 px-3 py-2 text-[11.5px] text-warning">
                <AlertTriangle size={12} /> One output is capped at its row
                limit, so rows it doesn't hold show as only on the other side.
              </p>
            )}
          </div>
        </>
      )}
      {editing && (
        <CompareDialog
          initialSpec={spec}
          initialSources={sources}
          onApply={(next, nextSources) => {
            setEditing(false);
            setSpec(next, nextSources);
          }}
          onClose={() => setEditing(false)}
        />
      )}
    </div>
  );
}

/** Before in red and after in green, as in the changed cells (`old → new`). */
const SIDE_TONE = {
  danger: { box: "border-danger/40 bg-danger/5", label: "text-danger" },
  success: { box: "border-success/40 bg-success/5", label: "text-success" },
} as const;

function Side({
  label,
  tone,
  o,
  fallback,
  rows,
  from,
}: {
  label: string;
  tone: keyof typeof SIDE_TONE;
  o?: CompareRun["before"];
  fallback: string;
  rows?: number;
  from: string | null;
}) {
  const t = SIDE_TONE[tone];
  return (
    <div
      className={`min-w-0 rounded-md border px-2 py-1 ${t.box}`}
      data-side={label.toLowerCase()}
    >
      <div
        className={`text-[10.5px] font-semibold uppercase tracking-wide ${t.label}`}
      >
        {label}
      </div>
      <div className="truncate font-medium" title={o?.sql}>
        {from ?? (o ? `${outputLabel(o)} (${o.handle})` : fallback)}
      </div>
      <div className="text-[11px] text-muted">
        {o ? o.connection_name : ""}
        {rows != null ? ` · ${formatCount(rows)} rows` : ""}
      </div>
    </div>
  );
}

function Count({
  label,
  n,
  cls,
  active,
  onClick,
}: {
  label: string;
  n: number;
  cls: string;
  active?: boolean;
  onClick?: () => void;
}) {
  const body = (
    <>
      <span className={`text-[15px] font-semibold tabular-nums ${cls}`}>
        {formatCount(n)}
      </span>
      <span className="text-[11.5px] text-muted">{label}</span>
    </>
  );
  if (!onClick)
    return (
      <span className="flex items-baseline gap-1.5 px-2 py-1">{body}</span>
    );
  return (
    <button
      className={`flex items-baseline gap-1.5 rounded-md border px-2 py-1 ${active ? "border-accent bg-accent/10" : "border-line hover:bg-hover"}`}
      onClick={onClick}
      disabled={n === 0}
      aria-pressed={active}
      title={n ? `Show only these rows (click again for all)` : undefined}
    >
      {body}
    </button>
  );
}

function Cell({ v }: { v: string | null | undefined }) {
  return v == null ? (
    <span className="italic text-muted/60">NULL</span>
  ) : (
    <>{v}</>
  );
}
