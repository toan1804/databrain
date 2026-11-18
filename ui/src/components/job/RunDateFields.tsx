// What `:run_date` / `:run_time` are in a step's SQL: the value its database
// gets for a run for a date, ways to write the day before (as a date and as
// text in common shapes), the SQL as it would run, and a tab to try it in.
import { useEffect, useState } from "react";
import { CalendarDays, Copy, ExternalLink } from "lucide-react";
import { api, isTauri, toError } from "../../lib/api";
import type { Job, JobNode, RunMomentPart, RunMomentPreview } from "../../lib/types";
import { engineName } from "../../lib/sourceTraits";
import { useStore } from "../../store";
import { isResults, label } from "./shared";
import { RUN_DATE, RUN_TIME, wallClockUse } from "./runDate";

/** `:run_date` or `:run_time` in `sql` (as a parameter, not inside a longer name). */
export function usesRunMoment(sql: string): boolean {
  return new RegExp(`(^|[^:\\w]):(${RUN_DATE}|${RUN_TIME})\\b`, "i").test(sql);
}

/** The step's SQL texts, by the preview part they are. */
function texts(node: JobNode): Record<RunMomentPart["what"], string> {
  return { step: node.sql, before: node.load_before_sql ?? "", after: node.load_after_sql ?? "" };
}

/** A text the panel says something about: it uses the run date, or today's. */
const concerns = (sql: string) => usesRunMoment(sql) || !!wallClockUse(sql);

export function RunDateFields({ job, node }: { job: Job; node: JobNode }) {
  const all = texts(node);
  const shown = Object.values(all).some(concerns);
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
  // Each text on the database it runs on: the step's own SQL on its
  // connection (Results for a load or export), before/after on the target.
  const tryIt = (part: RunMomentPart) => {
    if (!p || !part.sql.trim()) return;
    const conn = part.what !== "step" ? (node.target_connection_id ?? null) : node.kind === "query" && node.connection_id ? node.connection_id : (results?.id ?? null);
    const what = part.what === "step" ? "" : ` (${part.what} SQL)`;
    useStore.getState().newTab({ title: `${node.name}${what} for ${p.date}`, sql: part.sql, connection_id: conn });
  };
  const parts = (p?.parts ?? []).filter((x) => concerns(all[x.what]));
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
      {p &&
        parts.map((part) => (
          <PartView key={part.what} part={part} date={p.date} several={parts.length > 1 || part.what !== "step"} onCopy={copy} onTry={() => tryIt(part)} />
        ))}
    </section>
  );
}

const PART_NAME: Record<RunMomentPart["what"], string> = { step: "The step's SQL", before: "The before SQL", after: "The after SQL" };

function PartView({ part, date, several, onCopy, onTry }: { part: RunMomentPart; date: string; several: boolean; onCopy: (t: string) => void; onTry: () => void }) {
  return (
    <div className="space-y-1" aria-label={PART_NAME[part.what]}>
      {several && (
        <div className="font-medium">
          {PART_NAME[part.what]} <span className="font-normal text-muted">(runs on {engineName(part.kind)})</span>
        </div>
      )}
      <div>
        <code>:{RUN_DATE}</code> is <Code text={part.run_date} onCopy={onCopy} /> and <code>:{RUN_TIME}</code> is <Code text={part.run_time} onCopy={onCopy} />
        {part.text ? ": text, not a date" : ": a date and a time, not text"}
        {part.examples.length > 0 ? `, so write the day before and other shapes of it in ${part.text ? "its functions" : "SQL"}:` : "."}
      </div>
      {part.examples.length > 0 && (
        <table className="w-full">
          <tbody>
            {part.examples.map((ex) => (
              <tr key={ex.sql} className="align-top">
                <td className="whitespace-nowrap pr-2 text-muted">{ex.label}</td>
                <td className="w-full">
                  <Code text={ex.sql} onCopy={onCopy} />
                </td>
                <td className="whitespace-nowrap pl-2 font-mono" title={`For a run for ${date}`}>
                  → {ex.value}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {part.examples.some((ex) => /[A-Za-z]{3}-\d\d$/.test(ex.value)) && (
        <div className="text-muted">Month names follow the session's language (Oracle's NLS_DATE_LANGUAGE, MySQL's lc_time_names, SQL Server's LANGUAGE).</div>
      )}
      <details>
        <summary className="cursor-pointer text-muted hover:text-fg">
          {PART_NAME[part.what]} for {date}
        </summary>
        <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap break-words rounded bg-bg p-1.5 font-mono text-[11px]">{part.sql}</pre>
        <button className="btn-ghost mt-1 px-2 py-0.5 text-[12px]" onClick={onTry} title="A new tab with this SQL, to run it and see the rows">
          <ExternalLink size={12} /> Open in editor
        </button>
      </details>
    </div>
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
