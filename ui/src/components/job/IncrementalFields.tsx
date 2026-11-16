// Incremental step settings: the column whose largest value the job
// remembers (the mark), the start value, and the current mark with Reset.
import { useEffect, useId, useRef, useState } from "react";
import { RotateCcw } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { JobNode, Watermark } from "../../lib/types";
import { relativeTime } from "../../lib/util";
import { useStore } from "../../store";
import { field, label, useCommitOnUnmount } from "./shared";

/** How long typing in the column field waits before it is saved (and its mark looked up). */
export const COLUMN_SAVE_DELAY = 400;

/** The parameter an incremental step's SQL reads its mark from. */
export const WATERMARK = "watermark";

export const isIncremental = (n: Pick<JobNode, "incremental_column">) => !!n.incremental_column?.trim();

/** What's wrong with an incremental step's settings (`mark`: its current mark), or null. */
export function incrementalProblem(node: Pick<JobNode, "incremental_column" | "incremental_start" | "sql">, mark: Watermark | null): string | null {
  const col = node.incremental_column?.trim();
  if (!col) return null;
  if (!new RegExp(`:${WATERMARK}\\b`, "i").test(node.sql)) return `The SQL doesn't use :${WATERMARK}, so every run reads all rows: e.g. where ${col} > :${WATERMARK}`;
  if (!mark && !node.incremental_start?.trim()) return `Enter a start value: the first run reads the rows after it (:${WATERMARK} until the step has a mark).`;
  return null;
}

export function IncrementalFields({ jobId, node, onChange }: { jobId: string; node: JobNode; onChange: (patch: Partial<JobNode>) => void }) {
  const col = node.incremental_column?.trim() ?? "";
  const [on, setOn] = useState(!!col);
  // The column cleared or set elsewhere (the assistant, another window): the checkbox follows.
  const prevCol = useRef(col);
  useEffect(() => {
    if (prevCol.current && !col) setOn(false);
    if (!prevCol.current && col) setOn(true);
    prevCol.current = col;
  }, [col]);
  // The column as typed, saved a moment after the last keystroke (each
  // keystroke saved the job and looked its mark up).
  const [colText, setColText] = useState(node.incremental_column ?? "");
  const pending = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => {
    if (!pending.current) setColText(node.incremental_column ?? "");
  }, [node.incremental_column]);
  const saveCol = (text: string) => {
    clearTimeout(pending.current);
    pending.current = undefined;
    if ((text || undefined) !== (node.incremental_column || undefined)) onChange({ incremental_column: text || undefined });
  };
  useCommitOnUnmount(() => pending.current && saveCol(colText));
  const [mark, setMark] = useState<Watermark | null>(null);
  const [markError, setMarkError] = useState<string | null>(null);
  // Only the latest lookup counts: an older one answering late (after a
  // Reset, or for the column typed before) would show a mark that isn't.
  const lookup = useRef(0);
  const finished = node.last_run?.finished_at ?? 0;
  useEffect(() => {
    const id = ++lookup.current;
    setMarkError(null);
    if (!col) return setMark(null);
    api
      .jobWatermark(jobId, node.id, col)
      .then((m) => id === lookup.current && setMark(m))
      .catch((e) => {
        if (id !== lookup.current) return;
        setMark(null);
        setMarkError(toError(e).message);
      });
  }, [jobId, node.id, col, finished]);
  const reset = () =>
    useStore.getState().askConfirm({
      title: `Reset the mark of "${node.name}"?`,
      reasons: [
        `The next run reads from the start value${node.incremental_start ? ` (${node.incremental_start})` : ""} again, not from ${mark?.value ?? "the mark"}.`,
        "A load that appends rows then writes again the rows it already wrote.",
      ],
      confirmLabel: "Reset",
      onConfirm: () => {
        lookup.current++;
        api
          .resetJobWatermark(jobId, node.id)
          .then(() => {
            setMark(null);
            setMarkError(null);
          })
          .catch((e) => useStore.getState().toast(toError(e).message, "error"));
      },
    });
  const colId = useId();
  const startId = useId();
  const problem = incrementalProblem(node, mark);
  return (
    <section aria-label="Incremental" className="space-y-2 rounded-md border border-line p-2">
      <label className="flex items-center gap-1.5 text-[12px]">
        <input
          type="checkbox"
          checked={on}
          onChange={(e) => {
            setOn(e.target.checked);
            if (!e.target.checked) {
              clearTimeout(pending.current);
              pending.current = undefined;
              setColText("");
              onChange({ incremental_column: undefined, incremental_start: undefined });
            }
          }}
        />
        Incremental: read only the rows after the last run's
      </label>
      {on && (
        <>
          <div className="grid grid-cols-2 gap-2">
            <div>
              <label className={label} htmlFor={colId}>
                Column
              </label>
              <input
                id={colId}
                className={`${field} font-mono`}
                aria-label="Incremental column"
                placeholder="updated_at"
                value={colText}
                onChange={(e) => {
                  const text = e.target.value;
                  setColText(text);
                  clearTimeout(pending.current);
                  pending.current = setTimeout(() => saveCol(text), COLUMN_SAVE_DELAY);
                }}
                onBlur={() => pending.current && saveCol(colText)}
              />
            </div>
            <div>
              <label className={label} htmlFor={startId}>
                Start value
              </label>
              <input
                id={startId}
                className={`${field} font-mono`}
                aria-label="Incremental start value"
                placeholder="2026-01-01"
                value={node.incremental_start ?? ""}
                onChange={(e) => onChange({ incremental_start: e.target.value || undefined })}
              />
            </div>
          </div>
          <p className="text-[11px] text-muted">
            The SQL gets the largest {col || "column value"} of the last successful run as <code>:{WATERMARK}</code> (the start value until then), e.g.{" "}
            <code>
              where {col || "updated_at"} {">"} :{WATERMARK}
            </code>
            . It moves only when this step and every step after it succeed.
          </p>
          {col && (
            <div className="flex items-center gap-2 text-[11.5px]" aria-label="Incremental mark">
              {markError ? (
                <span className="min-w-0 flex-1 text-danger" role="alert">
                  The mark couldn't be read: {markError}
                </span>
              ) : mark ? (
                <span className="min-w-0 flex-1 truncate" title={`Taken from ${mark.column}`}>
                  Mark: <span className="font-mono">{mark.value}</span>
                  <span className="text-muted">
                    {mark.run_id != null ? ` · run #${mark.run_id}` : ""} · {relativeTime(mark.updated_at)}
                  </span>
                </span>
              ) : (
                <span className="min-w-0 flex-1 text-muted">No mark yet: the next run uses the start value.</span>
              )}
              {(mark || markError) && (
                <button className="btn-ghost shrink-0 py-0.5 text-[11px]" title="Forget the mark: the next run reads from the start value again" onClick={() => void reset()}>
                  <RotateCcw size={11} /> Reset
                </button>
              )}
            </div>
          )}
          {problem && <div className="rounded-md border border-warning/40 bg-warning/10 px-2 py-1 text-[11.5px]">{problem}</div>}
        </>
      )}
    </section>
  );
}
