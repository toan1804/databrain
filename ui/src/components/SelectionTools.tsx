// Selection tools of a result grid: the summary in the status bar (count,
// distinct, sum, min, max, avg; click one to copy it) and the transposed view
// of selected rows (one row per column).

import { useEffect, useState } from "react";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { Copy, Loader2 } from "lucide-react";
import { api, toError } from "../lib/api";
import type { ResultInfo, SelectionSummary, ViewSpec } from "../lib/types";
import { formatCount } from "../lib/util";
import { useStore } from "../store";
import { Modal } from "./ui";

/** Selected rectangle: columns (empty = all) and a row range of the view. */
export interface GridRange {
  cols: number[];
  offset: number;
  limit: number;
}

/** Numbers as a spreadsheet's status bar shows them (up to 6 decimals). */
export function formatStat(v: number): string {
  if (Number.isInteger(v)) return v.toLocaleString("en-US");
  return v.toLocaleString("en-US", { maximumFractionDigits: 6 });
}

/** The summary items shown for a selection (label, display, value to copy). */
export function summaryItems(s: SelectionSummary): { label: string; shown: string; copy: string }[] {
  const out = [
    { label: "Count", shown: formatCount(s.non_null), copy: String(s.non_null) },
    { label: "Distinct", shown: `${formatCount(s.distinct)}${s.distinct_capped ? "+" : ""}`, copy: String(s.distinct) },
  ];
  const num = (label: string, v: number | null) => v !== null && out.push({ label, shown: formatStat(v), copy: String(v) });
  num("Sum", s.sum);
  num("Avg", s.avg);
  num("Min", s.min);
  num("Max", s.max);
  return out;
}

export function SelectionSummaryBar({ info, view, range }: { info: ResultInfo; view: ViewSpec; range: GridRange | null }) {
  const toast = useStore((s) => s.toast);
  const [sum, setSum] = useState<SelectionSummary | null>(null);
  const cells = range ? (range.cols.length || info.columns.length) * range.limit : 0;
  const key = range ? `${range.cols.join(",")}:${range.offset}:${range.limit}` : "";
  useEffect(() => {
    setSum(null);
    if (!range || cells < 2) return;
    let live = true;
    const t = setTimeout(() => {
      api
        .summarizeSelection(info.id, view, range.cols, range.offset, range.limit)
        .then((s) => live && setSum(s))
        .catch(() => {});
    }, 150);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [info.id, view, key]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!sum) return null;
  return (
    <span className="flex min-w-0 items-center gap-2 overflow-hidden" aria-label="Selection summary">
      {summaryItems(sum).map((it) => (
        <button
          key={it.label}
          className="shrink-0 rounded px-1 hover:bg-hover hover:text-fg"
          title={`Copy ${it.label.toLowerCase()} (${it.copy})`}
          onClick={() =>
            void writeText(it.copy)
              .then(() => toast(`Copied ${it.label.toLowerCase()} ${it.copy}`, "success"))
              .catch((e) => toast(toError(e).message, "error"))
          }
        >
          {it.label} <span className="text-fg">{it.shown}</span>
        </button>
      ))}
    </span>
  );
}

/** Most rows shown side by side when transposing. */
export const TRANSPOSE_MAX = 50;

/** Selected rows turned around: a row per column, a column per selected row. */
export function TransposeDialog({ info, view, range, onClose }: { info: ResultInfo; view: ViewSpec; range: GridRange; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [rows, setRows] = useState<(string | null)[][] | null>(null);
  const n = Math.min(range.limit, TRANSPOSE_MAX);
  const cols = range.cols.length ? range.cols : info.columns.map((_, i) => i);
  useEffect(() => {
    api
      .fetchPage(info.id, view, range.offset, n)
      .then((p) => setRows(p.rows))
      .catch((e) => toast(toError(e).message, "error"));
  }, [info.id, view, range.offset, n, toast]);
  const copy = () => {
    if (!rows) return;
    const esc = (v: string | null) => (v ?? "").replace(/[\t\n\r]/g, " ");
    const head = ["column", ...rows.map((_, i) => `row ${range.offset + i + 1}`)].join("\t");
    const body = cols.map((c) => [info.columns[c].name, ...rows.map((r) => esc(r[c]))].join("\t"));
    void writeText([head, ...body].join("\n")).then(() => toast("Copied the transposed rows", "success"));
  };
  return (
    <Modal
      title={`${formatCount(n)} row${n === 1 ? "" : "s"} transposed`}
      onClose={onClose}
      width={Math.min(1100, 260 + n * 180)}
      footer={
        <>
          {range.limit > TRANSPOSE_MAX && <span className="mr-auto text-[11.5px] text-muted">The first {TRANSPOSE_MAX} selected rows</span>}
          <button className="btn-ghost" onClick={copy} disabled={!rows}>
            <Copy size={13} /> Copy
          </button>
          <button className="btn-primary" onClick={onClose}>
            Close
          </button>
        </>
      }
    >
      {!rows ? (
        <div className="flex justify-center p-6 text-muted">
          <Loader2 size={18} className="animate-spin" />
        </div>
      ) : (
        <div className="max-h-[60vh] overflow-auto">
          <table className="w-full border-collapse font-mono text-[12px]">
            <thead className="sticky top-0 bg-panel">
              <tr>
                <th className="border-b border-line px-2 py-1 text-left font-sans text-[11.5px] text-muted">Column</th>
                {rows.map((_, i) => (
                  <th key={i} className="border-b border-line px-2 py-1 text-left font-sans text-[11.5px] text-muted">
                    Row {formatCount(range.offset + i + 1)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {cols.map((c) => (
                <tr key={c} className="align-top hover:bg-hover">
                  <th scope="row" className="border-b border-line px-2 py-1 text-left font-sans font-medium">
                    {info.columns[c].name}
                  </th>
                  {rows.map((r, i) => (
                    <td key={i} className="max-w-[360px] whitespace-pre-wrap break-words border-b border-line px-2 py-1">
                      {r[c] === null ? <span className="text-muted">NULL</span> : r[c]}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Modal>
  );
}
