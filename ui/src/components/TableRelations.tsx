// Table explorer, step 3: Relations (foreign keys in and out, learned and
// documented joins, the ER diagram around the table) and Usage (DataBrain's
// own query history and job steps). Local reads, and the schema's catalog
// read the ER diagram uses; nothing is sent to the AI.
import { useEffect, useState, type ReactNode } from "react";
import { ArrowLeft, ArrowRight, FileCode2, Link2, Loader2, RefreshCw, Share2, Sparkles, SquareTerminal, Table2, Workflow } from "lucide-react";
import { api, isTauri, toError } from "../lib/api";
import type { ConnectionView, LearnedView, ObjectDetail, TableUsageView } from "../lib/types";
import type { TableColumns } from "../lib/bindings/TableColumns";
import type { Origin } from "../lib/bindings/Origin";
import { inboundKeys, joinsOf, queryUses } from "../lib/tableRelations";
import { traitsOf } from "../lib/sourceTraits";
import { formatCount, relativeTime, sqlPreview } from "../lib/util";
import { useStore, type DdlRef } from "../store";
import { useJobs } from "../jobsStore";
import { openErDiagram, openTable } from "./openTabs";

type Loaded<T> = { status: "loading" } | { status: "ready"; value: T } | { status: "error"; message: string };

/**
 * Load `read`, again when `deps` change, keeping only the latest answer.
 * What is shown stays while it is read again (no spinner, no jump), only
 * the first read shows "loading".
 */
function useLoaded<T>(read: () => Promise<T>, deps: unknown[]): Loaded<T> {
  const [v, setV] = useState<Loaded<T>>({ status: "loading" });
  useEffect(() => {
    let live = true;
    setV((old) => (old.status === "ready" ? old : { status: "loading" }));
    read()
      .then((value) => live && setV({ status: "ready", value }))
      .catch((e) => live && setV({ status: "error", message: toError(e).message }));
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- the caller lists what the read depends on
  }, deps);
  return v;
}

function Section({ title, aside, children }: { title: ReactNode; aside?: ReactNode; children: ReactNode }) {
  return (
    <section className="space-y-1.5">
      <h3 className="flex items-center gap-2 text-[11px] font-medium uppercase tracking-wide text-muted">
        {title}
        {aside && <span className="ml-auto normal-case tracking-normal">{aside}</span>}
      </h3>
      {children}
    </section>
  );
}

const Busy = ({ what }: { what: string }) => (
  <p className="flex items-center gap-1.5 text-muted">
    <Loader2 size={12} className="animate-spin" /> {what}
  </p>
);

/** A table name that opens its own explorer tab. */
function TableLink({ conn, schema, name }: { conn: ConnectionView; schema: string; name: string }) {
  return (
    <button className="font-mono text-accent hover:underline" onClick={() => openTable(conn, { schema, name, kind: "table" })} title={`Open ${schema}.${name}`}>
      {name}
    </button>
  );
}

/** `schema.name` as the knowledge base names it → the explorer's schema id and name (the last part). */
const splitFull = (full: string, fallbackSchema: string): [string, string] => {
  const i = full.lastIndexOf(".");
  return i > 0 ? [full.slice(0, i), full.slice(i + 1)] : [fallbackSchema, full];
};

/** Warehouses where a schema-wide catalog read is slow or billed: asked for. */
const CLOUD = new Set(["bigquery", "snowflake", "databricks"]);

const JOIN_STATE: Record<string, string> = { learned: "learned", confirmed: "approved", rejected: "rejected" };

/** Foreign keys in and out, learned and documented joins, and the ER diagram around the table. */
export function RelationsPart({ conn, reference, detail }: { conn: ConnectionView; reference: DdlRef; detail: ObjectDetail | null }) {
  const [seq, setSeq] = useState(0);
  const kind = conn.config.kind;
  const sql = traitsOf(kind).preview === "sql";
  // Reading every table's keys of a schema is slow, or billed, on the cloud
  // warehouses: asked for there (a button), read at once elsewhere.
  const onAsk = CLOUD.has(kind);
  const [asked, setAsked] = useState(!onAsk);
  const schemaTables = useLoaded<TableColumns[] | null>(
    async () => (!sql || !asked ? null : isTauri() ? (await api.schemaTables(conn.id, reference.schema)).tables : []),
    [conn.id, reference.schema, seq, asked],
  );
  const learned = useLoaded<LearnedView>(() => api.knLearned(conn.id), [conn.id, seq]);
  const out = detail?.foreign_keys ?? [];
  const inbound = schemaTables.status === "ready" && schemaTables.value ? inboundKeys(schemaTables.value, reference.schema, reference.name) : [];
  const joins = learned.status === "ready" ? joinsOf(learned.value.joins, reference.schema, reference.name).filter((j) => !j.join.declared) : [];
  return (
    <div className="h-full space-y-4 overflow-auto p-3 text-[12px]">
      <div className="flex flex-wrap items-center gap-2">
        <button className="btn-ghost border border-line py-0.5 text-[12px]" onClick={() => openErDiagram(conn.id, reference.schema, reference.schema, reference.name)} title="The schema's ER diagram, with this table's links traced">
          <Share2 size={12} /> ER diagram around it
        </button>
        <button className="btn-ghost py-0.5 text-[12px]" onClick={() => setSeq((n) => n + 1)} title="Read the schema's keys and the learned joins again">
          <RefreshCw size={12} /> Refresh
        </button>
      </div>

      <Section title={<><ArrowRight size={11} /> It refers to ({out.length})</>}>
        {!detail ? (
          <Busy what="Reading the table…" />
        ) : out.length === 0 ? (
          <p className="text-muted">No foreign keys of its own.</p>
        ) : (
          <ul className="space-y-0.5">
            {out.map((f) => (
              <li key={`${f.columns.join(",")}>${f.ref_schema}.${f.ref_table}`} className="flex flex-wrap items-center gap-1">
                <span className="font-mono">({f.columns.join(", ")})</span> → <TableLink conn={conn} schema={f.ref_schema || reference.schema} name={f.ref_table} />
                <span className="font-mono text-muted">({f.ref_columns.join(", ")})</span>
                {f.ref_schema && f.ref_schema !== reference.schema && <span className="text-muted">in {f.ref_schema}</span>}
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title={<><ArrowLeft size={11} /> Refer to it {schemaTables.status === "ready" && schemaTables.value ? `(${inbound.length})` : ""}</>} aside={sql && <span className="text-muted">tables of {reference.schema} only</span>}>
        {!sql ? (
          <p className="text-muted">{traitsOf(kind).name} has no foreign keys between {kind === "mongodb" ? "collections" : "labels"}.</p>
        ) : !asked ? (
          <p className="flex flex-wrap items-center gap-2 text-muted">
            Reading the keys of every table of {reference.schema} is slow{kind === "bigquery" ? " and billed" : ""} on {traitsOf(kind).name}.
            <button className="btn-ghost border border-line py-0.5 text-[12px]" onClick={() => setAsked(true)}>
              Find the tables that refer to it
            </button>
          </p>
        ) : schemaTables.status === "loading" ? (
          <Busy what={`Reading the keys of ${reference.schema}…`} />
        ) : schemaTables.status === "error" ? (
          <p className="text-danger">The schema's keys couldn't be read: {schemaTables.message}</p>
        ) : inbound.length === 0 ? (
          <p className="text-muted">No table of {reference.schema} has a foreign key to it.</p>
        ) : (
          <ul className="space-y-0.5">
            {inbound.map(({ table, fk }) => (
              <li key={`${table}:${fk.columns.join(",")}`} className="flex flex-wrap items-center gap-1">
                <TableLink conn={conn} schema={reference.schema} name={table} />
                <span className="font-mono text-muted">({fk.columns.join(", ")})</span> → <span className="font-mono">({fk.ref_columns.join(", ")})</span>
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title={<><Link2 size={11} /> Joins learned from your queries {learned.status === "ready" ? `(${joins.length})` : ""}</>} aside={<span className="text-muted">keys not declared</span>}>
        {learned.status === "loading" ? (
          <Busy what="Reading the knowledge base…" />
        ) : learned.status === "error" ? (
          <p className="text-danger">{learned.message}</p>
        ) : !learned.value.supported ? (
          <p className="text-muted">Joins aren't learned from {conn.name}'s queries (its query language isn't SQL).</p>
        ) : !learned.value.enabled ? (
          <p className="text-muted">Learning from query history is off for {conn.name} (Edit connection → AI).</p>
        ) : joins.length === 0 ? (
          <p className="text-muted">No join with it seen in your queries or jobs yet, other than its declared keys.</p>
        ) : (
          <ul className="space-y-1">
            {joins.map((j) => {
              const [s, n] = splitFull(j.other, reference.schema);
              return (
                <li key={j.join.id} className="flex flex-wrap items-center gap-1">
                  <span className="font-mono">({j.columns.join(", ")})</span> = <TableLink conn={conn} schema={s} name={n} />
                  <span className="font-mono text-muted">({j.otherColumns.join(", ")})</span>
                  <span className="text-muted">
                    · {JOIN_STATE[j.join.status] ?? j.join.status}
                    {j.join.documented ? ", documented" : ""} · {formatCount(j.join.queries)} {j.join.queries === 1 ? "query" : "queries"} on {formatCount(j.join.days)} {j.join.days === 1 ? "day" : "days"}
                  </span>
                  <span className={j.join.shown ? "text-accent" : "text-muted"} title={j.join.hidden_reason ?? undefined}>
                    · {j.join.shown ? "given to the AI" : "not given to the AI yet"}
                  </span>
                </li>
              );
            })}
          </ul>
        )}
      </Section>
    </div>
  );
}

const ORIGIN: Record<Origin, { label: string; icon: ReactNode }> = {
  user: { label: "you", icon: null },
  job: { label: "job", icon: <Workflow size={10} /> },
  ai: { label: "AI", icon: <Sparkles size={10} /> },
  mcp: { label: "MCP", icon: <Sparkles size={10} /> },
  explorer: { label: "explorer", icon: <Table2 size={10} /> },
  cli: { label: "terminal", icon: <SquareTerminal size={10} /> },
};

/** History entries the backend reads for Usage (`USAGE_HISTORY` in api/jobs.rs). */
const USAGE_HISTORY = 1000;

/** How the table is used in DataBrain: its queries in the history, and the job steps that read or load it. */
export function UsagePart({ conn, reference }: { conn: ConnectionView; reference: DdlRef }) {
  const [seq, setSeq] = useState(0);
  const historyVersion = useStore((s) => s.historyVersion);
  // A job saved (a new version) or added or deleted changes which steps read
  // it; the list itself is rebuilt at every refresh, so its versions are the key.
  const jobs = useJobs((s) => s.jobs.map((j) => `${j.id}:${j.updated_at}`).join(","));
  const data = useLoaded<TableUsageView>(() => api.tableUsage(conn.id, reference.schema, reference.name), [conn.id, reference.schema, reference.name, seq, historyVersion, jobs]);
  const learned = useLoaded<LearnedView>(() => api.knLearned(conn.id), [conn.id, seq]);
  const history = data.status === "ready" ? ({ status: "ready", value: data.value.history } as const) : data;
  const steps = data.status === "ready" ? ({ status: "ready", value: data.value.steps } as const) : data;
  const uses = history.status === "ready" ? queryUses(history.value, reference.name) : [];
  const runs = uses.reduce((n, u) => n + u.runs, 0);
  const usage = data.status === "ready" ? data.value.learned : null;
  const learning = learned.status === "ready" ? learned.value : null;
  const open = (sql: string) => useStore.getState().newTab({ title: reference.name, sql, connection_id: conn.id });
  return (
    <div className="h-full space-y-4 overflow-auto p-3 text-[12px]">
      <div className="flex flex-wrap items-center gap-2 text-muted">
        <span>From DataBrain's own query history and jobs, on this computer: nothing is asked of the database.</span>
        <button className="btn-ghost ml-auto py-0.5 text-[12px]" onClick={() => setSeq((n) => n + 1)}>
          <RefreshCw size={12} /> Refresh
        </button>
      </div>

      {learning && !learning.supported ? (
        <p className="text-muted">How often your queries read it isn't learned on {conn.name} (its query language isn't SQL); the history below still lists them.</p>
      ) : learning && !learning.enabled ? (
        <p className="text-muted">Learning from query history is off for {conn.name} (Edit connection → AI), so how often your queries read it isn't counted; the history below still lists them.</p>
      ) : learning && !usage ? (
        <p className="text-muted">Not seen read in your queries or jobs since learning started.</p>
      ) : null}
      {usage && (
        <p>
          Read by your queries and jobs: <b>{formatCount(usage.queries)}</b> different {usage.queries === 1 ? "query" : "queries"} on <b>{formatCount(usage.days)}</b>{" "}
          {usage.days === 1 ? "day" : "days"}, last {relativeTime(usage.last_seen)} <span className="text-muted">(learned from the history; the assistant's own queries aren't counted)</span>.
        </p>
      )}

      <Section title={<><Workflow size={11} /> Job steps {steps.status === "ready" ? `(${steps.value.length})` : ""}</>}>
        {steps.status === "loading" ? (
          <Busy what="Reading the jobs…" />
        ) : steps.status === "error" ? (
          <p className="text-danger">{steps.message}</p>
        ) : steps.value.length === 0 ? (
          <p className="text-muted">No job step reads or loads it (as its SQL is written; a procedure call or dynamic SQL isn't seen).</p>
        ) : (
          <ul className="space-y-0.5">
            {steps.value.map((s) => (
              <li key={`${s.job_id}:${s.step_id}`} className="flex flex-wrap items-center gap-1">
                <button className="text-accent hover:underline" onClick={() => useJobs.getState().open(s.job_id, s.job_name)}>
                  {s.job_name}
                </button>
                <span className="text-muted">›</span> <span className="font-mono">{s.step_name}</span>
                <span className="text-muted">· {s.writes && s.reads ? "reads and writes it" : s.writes ? "writes it" : "reads it"}</span>
                {!s.sure && (
                  <span className="text-warning" title="Named without its schema (or after a USE): it is this table when the session's default schema is this one">
                    · may be another schema's {reference.name}
                  </span>
                )}
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section
        title={<><FileCode2 size={11} /> Queries that mention it {history.status === "ready" ? `(${uses.length})` : ""}</>}
        aside={
          history.status === "ready" && (
            <span className="text-muted">
              {formatCount(runs)} runs, in the latest {formatCount(history.value.length)} history entries on {conn.name} whose SQL contains "{reference.name}"
              {history.value.length >= USAGE_HISTORY ? " (older ones aren't read)" : ""}
            </span>
          )
        }
      >
        {history.status === "loading" ? (
          <Busy what="Reading the history…" />
        ) : history.status === "error" ? (
          <p className="text-danger">{history.message}</p>
        ) : uses.length === 0 ? (
          <p className="text-muted">No query in the history on {conn.name} names it.</p>
        ) : (
          <ul className="space-y-1">
            {uses.slice(0, 50).map((u) => (
              <li key={u.sql} className="flex items-start gap-2 rounded border border-line/60 px-2 py-1">
                <code className="min-w-0 flex-1 truncate font-mono text-[11.5px]" title={u.sql}>
                  {sqlPreview(u.sql, 160)}
                </code>
                <span className="shrink-0 tabular-nums text-muted" title={`${u.runs} runs${u.failed ? `, ${u.failed} failed` : ""}`}>
                  ×{formatCount(u.runs)}
                  {u.failed > 0 && <span className="text-danger"> ({u.failed} failed)</span>}
                </span>
                <span className="flex shrink-0 items-center gap-1 text-muted">
                  {u.origins.map((o) => (
                    <span key={o} className="flex items-center gap-0.5" title={`Run by ${ORIGIN[o].label}`}>
                      {ORIGIN[o].icon}
                      {ORIGIN[o].label}
                    </span>
                  ))}
                </span>
                <span className="shrink-0 text-muted">{relativeTime(u.last)}</span>
                <button className="btn-ghost shrink-0 py-0 text-[11px]" onClick={() => open(u.sql)} title="A query tab with this SQL">
                  <FileCode2 size={11} /> Open
                </button>
              </li>
            ))}
          </ul>
        )}
        {uses.length > 50 && <p className="text-muted">The 50 used most are shown.</p>}
      </Section>
    </div>
  );
}
