// Knowledge → Learned: how tables join (from queries the user and jobs ran)
// and which tables are used most. One line per join, filtered by table and
// by status; a join opens to show its use, the queries it came from, and
// to keep it as a note. Whether the AI gets a join is measured from use;
// the only choice left to the user is to reject a wrong one (and undo it).
import { useEffect, useMemo, useState } from "react";
import { ChevronRight, History, Info, Loader2, NotebookPen, RefreshCw, RotateCcw, Search, Trash2, X } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { HistoryEntry, JoinStatus, LearnedJoinView, LearnedView } from "../../lib/types";
import { formatCount, relativeTime } from "../../lib/util";
import { useStore } from "../../store";

/** Joins shown at first and added by each "Show more". */
const JOIN_PAGE = 50;

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

/** Where a join stands for the AI. */
export type JoinState = "given" | "waiting" | "rejected";

export function joinState(j: Pick<LearnedJoinView, "shown" | "status">): JoinState {
  return j.status === "rejected" ? "rejected" : j.shown ? "given" : "waiting";
}

const STATES: { key: JoinState; label: string; dot: string; help: string }[] = [
  { key: "given", label: "Given to AI", dot: "bg-success", help: "The AI gets these joins: used in 2 different queries or on 2 different days" },
  { key: "waiting", label: "Waiting", dot: "bg-muted/60", help: "Not used enough yet: given to the AI once used in another query or on another day" },
  { key: "rejected", label: "Rejected", dot: "bg-danger", help: "Never given to the AI" },
];

/** Joins whose tables or columns contain every word of `q` (any case). */
export function filterJoins(joins: LearnedJoinView[], q: string, state: JoinState | "all"): LearnedJoinView[] {
  const words = q.toLowerCase().split(/\s+/).filter(Boolean);
  return joins.filter((j) => {
    if (state !== "all" && joinState(j) !== state) return false;
    if (!words.length) return true;
    const text = `${joinCondition(j)} ${j.left_table} ${j.right_table}`.toLowerCase();
    return words.every((w) => text.includes(w));
  });
}

/** The schema every table is in (`sales.`), to leave out of the lines; else "". */
export function commonSchema(tables: string[]): string {
  const prefix = (t: string) => (t.lastIndexOf(".") < 0 ? null : t.slice(0, t.lastIndexOf(".") + 1));
  const first = tables.length ? prefix(tables[0]) : null;
  return first && tables.every((t) => prefix(t) === first) ? first : "";
}

/** `schema.table` with the schema muted (or left out when it is `hide`). */
function TableName({ name, hide = "" }: { name: string; hide?: string }) {
  if (hide && name.startsWith(hide)) return <span className="text-fg">{name.slice(hide.length)}</span>;
  const dot = name.lastIndexOf(".");
  if (dot < 0) return <span className="text-fg">{name}</span>;
  return (
    <>
      <span className="text-muted">{name.slice(0, dot + 1)}</span>
      <span className="text-fg">{name.slice(dot + 1)}</span>
    </>
  );
}

function JoinLine({ j, hide }: { j: LearnedJoinView; hide: string }) {
  return (
    <span className="font-mono text-[11.5px]">
      {j.left_columns.map((c, i) => (
        <span key={i}>
          {i > 0 && <span className="px-1 font-sans text-[10.5px] text-muted">and</span>}
          <TableName name={j.left_table} hide={hide} />.<span className="text-accent">{c}</span>
          <span className="px-1 text-muted">=</span>
          <TableName name={j.right_table} hide={hide} />.<span className="text-accent">{j.right_columns[i] ?? "?"}</span>
        </span>
      ))}
    </span>
  );
}

export function LearnedSection({
  connId,
  view,
  reload,
  onMakeNote,
}: {
  connId: string;
  view: LearnedView | null;
  reload: () => void;
  onMakeNote: (target: string, body: string) => Promise<boolean | undefined>;
}) {
  const toast = useStore((s) => s.toast);
  const askConfirm = useStore((s) => s.askConfirm);
  const [part, setPart] = useState<"joins" | "tables">("joins");
  const [query, setQuery] = useState("");
  const [state, setState] = useState<JoinState | "all">("all");
  const [shown, setShown] = useState(JOIN_PAGE);
  const [busy, setBusy] = useState(false);
  const [open, setOpen] = useState<number | null>(null);
  const [evidence, setEvidence] = useState<{ id: number; entries: HistoryEntry[] | null } | null>(null);

  useEffect(() => {
    setShown(JOIN_PAGE);
    setOpen(null);
    setEvidence(null);
    setQuery("");
    setState("all");
  }, [connId]);
  useEffect(() => setShown(JOIN_PAGE), [query, state]);

  const joins = useMemo(() => view?.joins ?? [], [view]);
  const counts = useMemo(() => {
    const c: Record<JoinState, number> = { given: 0, waiting: 0, rejected: 0 };
    for (const j of joins) c[joinState(j)] += 1;
    return c;
  }, [joins]);
  const list = useMemo(() => filterJoins(joins, query, state), [joins, query, state]);
  // All in one schema (the usual case): say it once instead of on every line.
  const schema = useMemo(() => commonSchema([...joins.flatMap((j) => [j.left_table, j.right_table]), ...(view?.tables ?? []).map((t) => t.table)]), [joins, view]);
  const tables = useMemo(() => {
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    return (view?.tables ?? []).filter((t) => words.every((w) => t.table.toLowerCase().includes(w)));
  }, [view, query]);

  const learnNow = async () => {
    setBusy(true);
    try {
      const r = await api.knLearnNow(connId);
      toast(`Read ${formatCount(r.entries)} history entries`, "success");
      reload();
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
        "Learned joins (rejected ones too) and table usage are deleted.",
        "The query history itself stays; only queries run from now on are learned from.",
      ],
      confirmLabel: "Forget",
      onConfirm: async () => {
        try {
          await api.knForgetLearned(connId);
          reload();
        } catch (e) {
          toast(toError(e).message, "error");
        }
      },
    });
  const setStatus = async (j: LearnedJoinView, status: JoinStatus) => {
    try {
      await api.knSetJoinStatus(j.id, status);
      reload();
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };
  const toggle = (j: LearnedJoinView) => {
    if (open === j.id) return setOpen(null);
    setOpen(j.id);
    setEvidence({ id: j.id, entries: null });
    api
      .knJoinEvidence(j.id)
      .then((entries) => setEvidence((e) => (e?.id === j.id ? { id: j.id, entries } : e)))
      .catch((e) => toast(toError(e).message, "error"));
  };
  const makeNote = async (j: LearnedJoinView) => {
    const ok = await onMakeNote(`${j.left_table} & ${j.right_table}`, `Join key: ${joinCondition(j)}`);
    if (ok) toast("Saved as a note", "success");
  };

  if (!view) return <div className="flex items-center gap-1.5 text-[11.5px] text-muted"><Loader2 size={12} className="animate-spin" /> Loading…</div>;
  if (!view.supported) {
    return <p className="text-[11.5px] text-muted">Not available for this type of database: its queries aren't SQL or MongoDB aggregations, so joins can't be read from them.</p>;
  }
  if (!view.enabled) return <p className="text-[11.5px] text-muted">Off for this connection. Turn on “Learn from query history” in Edit connection → AI.</p>;

  const about =
    "Join keys from queries you and your jobs ran. The AI gets a join once it is in 2 different queries or used on 2 days; its own queries never count. Reject a join that is wrong to keep it from the AI. Most used tables rank higher when the AI looks for tables.";
  return (
    <div className="flex min-h-0 flex-col gap-2">
      <div className="flex flex-wrap items-center gap-1.5">
        <div className="flex rounded-md border border-line p-0.5 text-[11.5px]" role="tablist" aria-label="Learned">
          {(["joins", "tables"] as const).map((p) => (
            <button
              key={p}
              role="tab"
              aria-selected={part === p}
              className={`rounded px-2 py-0.5 ${part === p ? "bg-hover font-medium text-fg" : "text-muted hover:text-fg"}`}
              onClick={() => setPart(p)}
            >
              {p === "joins" ? "Joins" : "Tables"} <span className="text-muted">{formatCount(p === "joins" ? joins.length : view.tables.length)}</span>
            </button>
          ))}
        </div>
        <span className="text-muted" title={about}>
          <Info size={12} />
        </span>
        <span className="truncate text-[11px] text-muted">
          {schema && <span className="font-mono">{schema.slice(0, -1)} · </span>}
          {view.state ? `read ${relativeTime(view.state.learned_at)}` : "not read yet"}
        </span>
        <div className="ml-auto flex items-center">
          <button className="icon-btn" disabled={busy} onClick={() => void learnNow()} title="Learn now: read all the query history kept for this connection again" aria-label="Learn now">
            {busy ? <Loader2 size={13} className="animate-spin" /> : <RefreshCw size={13} />}
          </button>
          <button
            className="icon-btn"
            disabled={joins.length === 0 && view.tables.length === 0}
            onClick={forget}
            title="Forget what was learned on this connection…"
            aria-label="Forget learned"
          >
            <Trash2 size={13} />
          </button>
        </div>
      </div>
      {view.state?.error && <div className="text-[11.5px] text-danger">{view.state.error}</div>}

      <div className="relative">
        <Search size={12} className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-muted" />
        <input
          className="field h-7 w-full py-0 pl-6 text-[11.5px]"
          placeholder={part === "joins" ? "Filter by table or column" : "Filter tables"}
          aria-label="Filter learned"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
      </div>

      {part === "joins" ? (
        <>
          <div className="flex flex-wrap gap-1 text-[11px]">
            {[{ key: "all" as const, label: "All", dot: "", help: "Every learned join" }, ...STATES].map((s) => {
              const n = s.key === "all" ? joins.length : counts[s.key];
              return (
                <button
                  key={s.key}
                  className={`flex items-center gap-1 rounded-full border px-2 py-0.5 ${state === s.key ? "border-accent bg-accent/10 text-fg" : "border-line text-muted hover:text-fg"}`}
                  title={s.help}
                  aria-pressed={state === s.key}
                  onClick={() => setState(s.key)}
                >
                  {s.dot && <span className={`h-1.5 w-1.5 rounded-full ${s.dot}`} />}
                  {s.label} <span className="tabular-nums">{formatCount(n)}</span>
                </button>
              );
            })}
          </div>

          {joins.length === 0 ? (
            <p className="text-[11.5px] text-muted">Nothing learned yet. Joins appear here after you run queries that join tables.</p>
          ) : list.length === 0 ? (
            <p className="text-[11.5px] text-muted">No join matches.</p>
          ) : (
            <div className="divide-y divide-line/60 rounded-md border border-line" role="list" aria-label="Learned joins">
              {list.slice(0, shown).map((j) => {
                const st = joinState(j);
                const meta = STATES.find((s) => s.key === st)!;
                const isOpen = open === j.id;
                return (
                  <div key={j.id} role="listitem" className={st === "rejected" ? "opacity-60" : ""}>
                    <div className="group flex items-center gap-1.5 px-2 py-1 hover:bg-hover">
                      <button
                        className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
                        onClick={() => toggle(j)}
                        aria-expanded={isOpen}
                        title={`${joinCondition(j)}\n${joinUsage(j)}\n${j.shown ? "Given to the AI" : (j.hidden_reason ?? meta.label)}`}
                      >
                        <ChevronRight size={11} className={`shrink-0 text-muted transition-transform ${isOpen ? "rotate-90" : ""}`} />
                        <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${meta.dot}`} aria-label={meta.label} />
                        <span className="min-w-0 truncate">
                          <JoinLine j={j} hide={schema} />
                        </span>
                      </button>
                      <span className="shrink-0 text-[10.5px] tabular-nums text-muted" title={joinUsage(j)}>
                        {formatCount(j.queries)}×
                      </span>
                      <div className="hidden shrink-0 items-center group-focus-within:flex group-hover:flex">
                        {j.status === "rejected" ? (
                          <button className="icon-btn h-5 w-5" onClick={() => void setStatus(j, "learned")} title="Undo reject: decide by how often it is used" aria-label="Undo reject">
                            <RotateCcw size={11} />
                          </button>
                        ) : (
                          <button className="icon-btn h-5 w-5" onClick={() => void setStatus(j, "rejected")} title="Reject: a wrong join, never give it to the AI" aria-label="Reject">
                            <X size={12} />
                          </button>
                        )}
                      </div>
                    </div>
                    {isOpen && (
                      <div className="space-y-1.5 bg-panel-2/60 px-2 pb-2 pl-6 pt-1 text-[11px]">
                        <div className="break-all font-mono text-[11px]">{joinCondition(j)}</div>
                        <div className="text-muted">{joinUsage(j)}</div>
                        <div>
                          {j.shown ? (
                            <span className="rounded bg-success/10 px-1 text-success">Given to the AI</span>
                          ) : (
                            <span className="rounded bg-panel px-1 text-muted">{j.hidden_reason ?? meta.label}</span>
                          )}
                        </div>
                        <div className="flex flex-wrap gap-1">
                          {j.status !== "rejected" && (
                            <button className="btn-ghost border border-line py-0 text-[11px]" onClick={() => void setStatus(j, "rejected")} title="A wrong join: never give it to the AI">
                              <X size={11} /> Reject
                            </button>
                          )}
                          {/* Confirmed by hand in an earlier version: back to measuring. */}
                          {j.status !== "learned" && (
                            <button className="btn-ghost border border-line py-0 text-[11px]" onClick={() => void setStatus(j, "learned")} title="Decide by how often it is used">
                              <RotateCcw size={11} /> {j.status === "rejected" ? "Undo reject" : "Measure from use"}
                            </button>
                          )}
                          <button className="btn-ghost border border-line py-0 text-[11px]" onClick={() => void makeNote(j)} title="Keep it as a note (notes can be exported and shared)">
                            <NotebookPen size={11} /> Make note
                          </button>
                        </div>
                        <div>
                          <div className="mb-0.5 flex items-center gap-1 text-muted">
                            <History size={11} /> Queries it came from
                          </div>
                          {evidence?.id !== j.id || evidence.entries === null ? (
                            <Loader2 size={11} className="animate-spin text-muted" />
                          ) : evidence.entries.length === 0 ? (
                            <div className="text-muted">They are no longer in the history.</div>
                          ) : (
                            <div className="space-y-1">
                              {evidence.entries.map((h) => (
                                <div key={h.id}>
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
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          )}
          {list.length > shown && (
            <button className="btn-ghost self-start py-0.5 text-[11.5px]" onClick={() => setShown((n) => n + JOIN_PAGE)}>
              Show {formatCount(Math.min(JOIN_PAGE, list.length - shown))} more of {formatCount(list.length - shown)}
            </button>
          )}
        </>
      ) : view.tables.length === 0 ? (
        <p className="text-[11.5px] text-muted">No table use learned yet.</p>
      ) : (
        <div className="rounded-md border border-line" role="list" aria-label="Most used tables">
          <div className="flex px-2 py-1 text-[10.5px] text-muted">
            <span className="flex-1">Most used tables (they rank higher when the AI looks for tables)</span>
            <span className="w-14 text-right">Queries</span>
            <span className="w-12 text-right">Days</span>
          </div>
          {tables.map((t) => (
            <div key={t.table} role="listitem" className="flex items-center border-t border-line/60 px-2 py-1 text-[11.5px]" title={`Last used ${relativeTime(t.last_seen)}`}>
              <span className="min-w-0 flex-1 truncate font-mono">
                <TableName name={t.table} hide={schema} />
              </span>
              <span className="w-14 text-right tabular-nums">{formatCount(t.queries)}</span>
              <span className="w-12 text-right tabular-nums text-muted">{formatCount(t.days)}</span>
            </div>
          ))}
          {tables.length === 0 && <div className="border-t border-line/60 px-2 py-1 text-[11px] text-muted">No table matches.</div>}
        </div>
      )}
    </div>
  );
}
