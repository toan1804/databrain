// Knowledge → Learned from query history: how tables join (from queries
// the user and jobs ran) and which tables are used most. The user confirms
// or rejects joins, sees the queries they came from, or keeps one as a note.
import { useCallback, useEffect, useState } from "react";
import { Check, History, Loader2, NotebookPen, RefreshCw, RotateCcw, Trash2, X } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { HistoryEntry, JoinStatus, LearnedJoinView, LearnedView } from "../../lib/types";
import { formatCount, relativeTime } from "../../lib/util";
import { useStore } from "../../store";

/** Joins shown at first and added by each "Show more". */
const JOIN_PAGE = 20;

/** `a.x = b.y AND a.z = b.w` */
export function joinCondition(j: Pick<LearnedJoinView, "left_table" | "left_columns" | "right_table" | "right_columns">): string {
  return j.left_columns.map((c, i) => `${j.left_table}.${c} = ${j.right_table}.${j.right_columns[i] ?? "?"}`).join(" AND ");
}

/** "3 queries · 2 days · usually LEFT JOIN" */
export function joinUsage(j: LearnedJoinView, now = Date.now()): string {
  const parts = [`${formatCount(j.queries)} ${j.queries === 1 ? "query" : "queries"}`, `${formatCount(j.days)} ${j.days === 1 ? "day" : "days"}`];
  if (j.last_seen != null) parts.push(`last ${relativeTime(j.last_seen, now)}`);
  const kinds = Object.entries(j.kinds).filter((e): e is [string, number] => typeof e[1] === "number");
  const usual = kinds.sort((a, b) => b[1] - a[1])[0]?.[0];
  if (usual === "left" || usual === "right" || usual === "full") parts.push(`usually ${usual.toUpperCase()} JOIN`);
  if (j.ai_queries > 0) parts.push(`AI: ${formatCount(j.ai_queries)}`);
  return parts.join(" · ");
}

export function LearnedSection({
  connId,
  version,
  onMakeNote,
}: {
  connId: string;
  version: number;
  onMakeNote: (target: string, body: string) => Promise<boolean | undefined>;
}) {
  const toast = useStore((s) => s.toast);
  const askConfirm = useStore((s) => s.askConfirm);
  const [view, setView] = useState<LearnedView | null>(null);
  const [shown, setShown] = useState(JOIN_PAGE);
  const [busy, setBusy] = useState(false);
  const [evidence, setEvidence] = useState<{ id: number; entries: HistoryEntry[] } | null>(null);

  const load = useCallback(() => {
    api
      .knLearned(connId)
      .then(setView)
      .catch((e) => toast(toError(e).message, "error"));
  }, [connId, toast]);
  useEffect(load, [load, version]);
  useEffect(() => {
    setShown(JOIN_PAGE);
    setEvidence(null);
  }, [connId]);

  const learnNow = async () => {
    setBusy(true);
    try {
      const r = await api.knLearnNow(connId);
      toast(`Read ${formatCount(r.entries)} history entries`, "success");
      load();
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(false);
    }
  };
  const forget = () =>
    askConfirm({
      title: "Forget what was learned on this connection?",
      reasons: [
        "Learned joins (confirmed and rejected ones too) and table usage are deleted.",
        "The query history itself stays; only queries run from now on are learned from.",
      ],
      confirmLabel: "Forget",
      onConfirm: async () => {
        try {
          await api.knForgetLearned(connId);
          load();
        } catch (e) {
          toast(toError(e).message, "error");
        }
      },
    });
  const setStatus = async (j: LearnedJoinView, status: JoinStatus) => {
    try {
      await api.knSetJoinStatus(j.id, status);
      load();
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };
  const showEvidence = async (j: LearnedJoinView) => {
    if (evidence?.id === j.id) return setEvidence(null);
    try {
      setEvidence({ id: j.id, entries: await api.knJoinEvidence(j.id) });
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };
  const makeNote = async (j: LearnedJoinView) => {
    const ok = await onMakeNote(`${j.left_table} & ${j.right_table}`, `Join key: ${joinCondition(j)}`);
    if (ok) toast("Saved as a note", "success");
  };

  if (!view) return null;
  const joins = view.joins;
  return (
    <div>
      <div className="mb-1 flex flex-wrap items-center gap-1">
        <span className="text-[11px] font-semibold uppercase tracking-wide text-muted">Learned from query history</span>
        {joins.length > 0 && <span className="text-[11px] text-muted">{formatCount(joins.length)}</span>}
        {view.supported && view.enabled && (
          <>
            <button className="btn-ghost ml-auto py-0.5 text-[11.5px]" disabled={busy} onClick={() => void learnNow()} title="Read all the query history kept for this connection again">
              {busy ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />} Learn now
            </button>
            <button className="btn-ghost py-0.5 text-[11.5px]" disabled={joins.length === 0 && view.tables.length === 0} onClick={forget} title="Delete what was learned on this connection">
              <Trash2 size={12} /> Forget…
            </button>
          </>
        )}
      </div>
      {!view.supported ? (
        <p className="text-[11.5px] text-muted">Not available for this type of database: its queries aren't SQL or MongoDB aggregations, so joins can't be read from them.</p>
      ) : !view.enabled ? (
        <p className="text-[11.5px] text-muted">Off for this connection. Turn on “Learn from query history” in Edit connection → AI.</p>
      ) : (
        <>
          <p className="mb-1.5 text-[11px] text-muted">
            Join keys from queries you and your jobs ran. The AI gets a join once it is in 2 different queries or used on 2 days, or when you confirm it; its own queries
            never count.
            {view.state && ` Last read ${relativeTime(view.state.learned_at)}.`}
          </p>
          {view.state?.error && <div className="mb-1 text-[11.5px] text-danger">{view.state.error}</div>}
          {joins.length === 0 && <div className="text-[11px] text-muted">Nothing learned yet. Joins appear here after you run queries that join tables.</div>}
          <div className="space-y-1">
            {joins.slice(0, shown).map((j) => (
              <div key={j.id} className={`rounded-md px-2 py-1.5 ${j.status === "rejected" ? "bg-panel-2 opacity-60" : "bg-panel-2"}`}>
                <div className="break-all font-mono text-[11.5px]">{joinCondition(j)}</div>
                <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[11px] text-muted">
                  <span>{joinUsage(j)}</span>
                  {j.shown ? (
                    <span className="rounded bg-success/10 px-1 text-success">{j.status === "confirmed" ? "Confirmed · given to the AI" : "Given to the AI"}</span>
                  ) : (
                    <span className="rounded bg-panel px-1" title={j.hidden_reason ?? undefined}>
                      {j.hidden_reason}
                    </span>
                  )}
                </div>
                <div className="mt-1 flex flex-wrap gap-1">
                  {j.status !== "confirmed" && (
                    <button className="btn-ghost py-0 text-[11px]" onClick={() => void setStatus(j, "confirmed")} title="Give it to the AI however often it was used">
                      <Check size={11} /> Confirm
                    </button>
                  )}
                  {j.status !== "rejected" && (
                    <button className="btn-ghost py-0 text-[11px]" onClick={() => void setStatus(j, "rejected")} title="Never give it to the AI">
                      <X size={11} /> Reject
                    </button>
                  )}
                  {j.status !== "learned" && (
                    <button className="btn-ghost py-0 text-[11px]" onClick={() => void setStatus(j, "learned")} title="Undo: decide by how often it is used">
                      <RotateCcw size={11} /> Reset
                    </button>
                  )}
                  <button className="btn-ghost py-0 text-[11px]" onClick={() => void showEvidence(j)} aria-expanded={evidence?.id === j.id}>
                    <History size={11} /> Queries
                  </button>
                  <button className="btn-ghost py-0 text-[11px]" onClick={() => void makeNote(j)} title="Keep it as a note (notes can be exported and shared)">
                    <NotebookPen size={11} /> Make note
                  </button>
                </div>
                {evidence?.id === j.id && (
                  <div className="mt-1 space-y-1 border-t border-line pt-1">
                    {evidence.entries.length === 0 && <div className="text-[11px] text-muted">The queries it came from are no longer in the history.</div>}
                    {evidence.entries.map((h) => (
                      <div key={h.id} className="text-[11px]">
                        <span className="text-muted">
                          {relativeTime(h.started_at)}
                          {h.origin === "ai" || h.origin === "mcp" ? " · run by the AI" : h.origin === "job" ? " · job" : ""}
                        </span>
                        <pre className="line-clamp-3 whitespace-pre-wrap break-all font-mono text-[11px]">{h.sql}</pre>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            ))}
            {joins.length > shown && (
              <button className="btn-ghost py-0.5 text-[11.5px]" onClick={() => setShown((n) => n + JOIN_PAGE)}>
                Show {formatCount(Math.min(JOIN_PAGE, joins.length - shown))} more of {formatCount(joins.length - shown)}
              </button>
            )}
          </div>
          {view.tables.length > 0 && (
            <div className="mt-2">
              <div className="mb-0.5 text-[11px] text-muted">Most used tables (they rank higher when the AI looks for tables)</div>
              <div className="flex flex-wrap gap-1">
                {view.tables.slice(0, 10).map((t) => (
                  <span key={t.table} className="rounded bg-panel-2 px-1.5 py-0.5 font-mono text-[11px]" title={`${t.queries} queries on ${t.days} days, last ${relativeTime(t.last_seen)}`}>
                    {t.table} <span className="text-muted">{formatCount(t.queries)}</span>
                  </span>
                ))}
              </div>
            </div>
          )}
        </>
      )}
    </div>
  );
}
