// What `:run_date` / `:run_time` are in a step's SQL: the value its database
// gets for a run for a date, ways to write the day before (as a date and as
// text in common shapes), the SQL as it would run, and a tab to try it in.
import { useEffect, useState } from "react";
import { CalendarDays, Copy, ExternalLink } from "lucide-react";
import { api, isTauri, toError } from "../../lib/api";
import type { Job, JobNode, RunMomentPreview } from "../../lib/types";
import { useStore } from "../../store";
import { isResults, label } from "./shared";
import { RUN_DATE, RUN_TIME, wallClockUse } from "./runDate";

/** `:run_date` or `:run_time` in `sql` (as a parameter, not inside a longer name). */
export function usesRunMoment(sql: string): boolean {
  return new RegExp(`(^|[^:\\w]):(${RUN_DATE}|${RUN_TIME})\\b`, "i").test(sql);
}

export function RunDateFields({ job, node }: { job: Job; node: JobNode }) {
  const sql = [node.sql, node.load_before_sql, node.load_after_sql].filter(Boolean).join("\n");
  const shown = usesRunMoment(sql) || !!wallClockUse(sql);
  const [date, setDate] = useState("");
  const [p, setP] = useState<RunMomentPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const results = useStore((s) => s.connections.find(isResults));
  const toast = useStore((s) => s.toast);
  useEffect(() => {
    if (!shown || !isTauri()) return;
    let gone = false;
    // After typing stops: the step as edited, not as saved.
    const t = setTimeout(() => {
      const edited = { ...job, nodes: job.nodes.map((n) => (n.id === node.id ? node : n)) };
      api
        .runMomentPreview(edited, node.id, date || null)
        .then((r) => !gone && (setP(r), setError(null)))
        .catch((e) => !gone && setError(toError(e).message));
    }, 300);
    return () => {
      gone = true;
      clearTimeout(t);
    };
  }, [shown, job, node, date]);
  if (!shown) return null;
  const copy = (text: string) => void navigator.clipboard?.writeText(text).then(() => toast("Copied", "success"));
  const tryIt = () => {
    if (!p?.sql.trim()) return;
    const conn = node.kind === "query" && node.connection_id ? node.connection_id : (results?.id ?? null);
    useStore.getState().newTab({ title: `${node.name} for ${p.date}`, sql: p.sql, connection_id: conn });
  };
  return (
    <section aria-label="Run date" className="space-y-1.5 rounded-md border border-line px-2 py-1.5 text-[11.5px]">
      <div className="flex items-center gap-2">
        <CalendarDays size={12} className="shrink-0 text-accent" />
        <span className={`${label} mb-0 flex-1`}>:{RUN_DATE} in this step</span>
        <label className="flex items-center gap-1 text-muted">
          for a run for
          <input
            type="date"
            aria-label="Run for date"
            className="rounded border border-line bg-bg px-1 py-0.5 text-[11.5px]"
            value={date || p?.date || ""}
            onChange={(e) => setDate(e.target.value)}
          />
        </label>
      </div>
      {error && <div className="text-danger">{error}</div>}
      {p && (
        <>
          <div>
            <code>:{RUN_DATE}</code> is <Code text={p.run_date} onCopy={copy} /> and <code>:{RUN_TIME}</code> is <Code text={p.run_time} onCopy={copy} />: a date and a time
            {p.kind === "sqlite" ? " as text" : `, not text`}, so write the day before and other shapes of it in SQL:
          </div>
          {p.examples.length > 0 && (
            <table className="w-full">
              <tbody>
                {p.examples.map((ex) => (
                  <tr key={ex.sql} className="align-top">
                    <td className="whitespace-nowrap pr-2 text-muted">{ex.label}</td>
                    <td className="w-full">
                      <Code text={ex.sql} onCopy={copy} />
                    </td>
                    <td className="whitespace-nowrap pl-2 font-mono" title={`For a run for ${p.date}`}>
                      → {ex.value}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {p.examples.some((ex) => /[A-Za-z]{3}-\d\d$/.test(ex.value)) && (
            <div className="text-muted">Month names follow the session's language (Oracle's NLS_DATE_LANGUAGE, MySQL's lc_time_names, SQL Server's LANGUAGE).</div>
          )}
          {p.sql.trim() && (
            <details>
              <summary className="cursor-pointer text-muted hover:text-fg">The step's SQL for {p.date}</summary>
              <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap break-words rounded bg-bg p-1.5 font-mono text-[11px]">{p.sql}</pre>
              <button className="btn-ghost mt-1 px-2 py-0.5 text-[12px]" onClick={tryIt} title="A new tab with this SQL, to run it and see the rows">
                <ExternalLink size={12} /> Open in editor
              </button>
            </details>
          )}
        </>
      )}
    </section>
  );
}

function Code({ text, onCopy }: { text: string; onCopy: (t: string) => void }) {
  return (
    <button className="group inline-flex max-w-full items-start gap-1 text-left font-mono text-[11px] hover:text-accent" onClick={() => onCopy(text)} title="Copy">
      <code className="break-all">{text}</code>
      <Copy size={10} className="mt-0.5 shrink-0 opacity-0 group-hover:opacity-100" />
    </button>
  );
}
