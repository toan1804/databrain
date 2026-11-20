// Table explorer, step 4: Indexes (the indexes, partitioning and clustering
// the connector reads for the editor's query tips, the checks and triggers
// from the catalog, and index use where the database keeps it) and History
// (Delta versions; Snowflake and BigQuery time travel). Catalog reads only:
// earlier rows open in a query tab, run when the user runs it.
import { useEffect, useRef, useState } from "react";
import { Clock, FileCode2, Key, Loader2, RefreshCw } from "lucide-react";
import { api, toError } from "../lib/api";
import { asOfSql, AS_OF_TIME, checksSql, historyKind, historySql, indexUseSql, num, readConstraints, rowsWritten, triggersSql, type Constraint } from "../lib/explore";
import { traitsOf } from "../lib/sourceTraits";
import type { TableLayout } from "../lib/types";
import { formatBytes, formatCount } from "../lib/util";
import { useStore } from "../store";
import { localFrom, RanSql, useExploreRead, type Explore } from "./tableExplore";

const Busy = ({ what }: { what: string }) => (
  <p className="flex items-center gap-1.5 text-muted">
    <Loader2 size={12} className="animate-spin" /> {what}
  </p>
);

/** Read once when the part opens (catalog reads: cheap, no question). */
function useOnce(run: () => void) {
  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    run();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once, when the part opens; Refresh reads again
  }, []);
}

/** The check form that ran last, by connection (see `checksSql`). */
const checkForm = new Map<string, number>();

type Layout = { status: "loading" } | { status: "ready"; layout: TableLayout } | { status: "error"; message: string };

/** Indexes, partitioning and clustering, checks and triggers, index use. */
export function IndexesPart({ explore: e }: { explore: Explore }) {
  const { conn, reference } = e;
  const kind = conn.config.kind;
  const { local } = localFrom(e);
  const [layout, setLayout] = useState<Layout>({ status: "loading" });
  const checks = useExploreRead(e, "checks");
  const trig = useExploreRead(e, "triggers");
  const use = useExploreRead(e, "index-use");
  const checkForms = checksSql(kind, reference.schema, reference.name, local);
  const trigSql = triggersSql(kind, reference.schema, reference.name, local);
  const useSql = indexUseSql(kind, reference.schema, reference.name);
  const busy = layout.status === "loading" || checks.ran.status === "running" || trig.ran.status === "running";
  const load = async () => {
    if (busy && layout.status !== "loading") return;
    setLayout({ status: "loading" });
    api
      .tableLayout(conn.id, reference.schema, reference.name)
      .then((l) => setLayout({ status: "ready", layout: l }))
      .catch((x) => setLayout({ status: "error", message: toError(x).message }));
    if ("sql" in trigSql) void trig.read(trigSql.sql, null);
    if ("sql" in useSql && reference.kind !== "view") void use.read(useSql.sql, null);
    // The forms in order until one runs (MariaDB's then MySQL's; Oracle 12.2+
    // then older), starting at the one that ran last on this connection, so a
    // MySQL server fails MariaDB's form once, not at every open.
    const first = checkForm.get(conn.id) ?? 0;
    for (let i = first; i < checkForms.length; i++) {
      const form = checkForms[i];
      if ("sql" in form && (await checks.read(form.sql, null))) {
        checkForm.set(conn.id, i);
        break;
      }
    }
  };
  useOnce(() => void load());
  const l = layout.status === "ready" ? layout.layout : null;
  const usage = new Map<string, { scans: number | null; bytes: number | null }>();
  if (use.ran.status === "done") for (const r of use.ran.rows.rows) usage.set((r[0] ?? "").toLowerCase(), { scans: num(r[1]), bytes: num(r[2]) });
  const constraints: Constraint[] = [
    ...(checks.ran.status === "done" ? readConstraints(kind, checks.ran.rows) : []),
    ...(trig.ran.status === "done" ? readConstraints(kind, trig.ran.rows) : []),
  ];
  const checksWhy = "unavailable" in checkForms[0] ? checkForms[0].unavailable : null;
  const trigWhy = "unavailable" in trigSql ? trigSql.unavailable : null;
  return (
    <div className="flex h-full min-h-0 flex-col text-[12px]">
      <div className="min-h-0 flex-1 space-y-4 overflow-auto p-3">
        <div className="flex items-center gap-2">
          <button className="btn-ghost border border-line py-0.5 text-[12px]" onClick={() => void load()} disabled={busy} title="Read the indexes, constraints and triggers again">
            <RefreshCw size={12} className={busy ? "animate-spin" : ""} /> Refresh
          </button>
        </div>
        <section className="space-y-1.5">
          <h3 className="text-[11px] font-medium uppercase tracking-wide text-muted">Indexes {l ? `(${l.indexes.length})` : ""}</h3>
          {layout.status === "loading" ? (
            <Busy what="Reading the indexes…" />
          ) : layout.status === "error" ? (
            <p className="text-danger">The indexes couldn't be read: {layout.message}</p>
          ) : !l || l.indexes.length === 0 ? (
            <p className="text-muted">{reference.kind === "view" ? "A view has no indexes of its own." : "No indexes."}</p>
          ) : (
            <table className="w-full border-collapse">
              <thead className="text-left text-[11px] uppercase tracking-wide text-muted">
                <tr>
                  <th className="px-2 py-1 font-medium">Name</th>
                  <th className="px-2 py-1 font-medium">Columns</th>
                  <th className="px-2 py-1 font-medium">Kind</th>
                  {usage.size > 0 && <th className="px-2 py-1 text-right font-medium" title="Index scans on this server since its statistics were last reset (a replica counts its own)">Scans</th>}
                  {usage.size > 0 && <th className="px-2 py-1 text-right font-medium">Size</th>}
                </tr>
              </thead>
              <tbody>
                {l.indexes.map((ix) => {
                  const u = usage.get(ix.name.toLowerCase());
                  return (
                    <tr key={ix.name} className="border-t border-line/60">
                      <td className="px-2 py-1 font-mono">{ix.name}</td>
                      <td className="px-2 py-1 font-mono">{ix.columns.join(", ")}</td>
                      <td className="px-2 py-1">
                        <span className="flex items-center gap-1">
                          {ix.primary ? (
                            <span className="flex items-center gap-0.5 text-warning">
                              <Key size={11} /> primary key
                            </span>
                          ) : ix.unique ? (
                            "unique"
                          ) : (
                            "index"
                          )}
                          {ix.method && <span className="text-muted">· {ix.method}</span>}
                        </span>
                      </td>
                      {usage.size > 0 && (
                        // A key does its work (uniqueness) without scans: only a plain index unused is worth a look.
                        <td
                          className={`px-2 py-1 text-right tabular-nums ${u?.scans === 0 && !ix.unique ? "text-warning" : ""}`}
                          title={u?.scans === 0 && !ix.unique ? "Never used for a scan on this server since its statistics were last reset" : undefined}
                        >
                          {u?.scans != null ? formatCount(u.scans) : ""}
                        </td>
                      )}
                      {usage.size > 0 && <td className="px-2 py-1 text-right tabular-nums">{u?.bytes != null ? formatBytes(u.bytes) : ""}</td>}
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
          {"unavailable" in useSql && l && l.indexes.length > 0 && <p className="text-[11px] text-muted">How often each index is used isn't read on {traitsOf(kind).name}.</p>}
          {use.ran.status === "error" && <p className="text-[11px] text-muted">Index use couldn't be read: {use.ran.message}</p>}
        </section>

        {l && (l.partition_by.length > 0 || l.cluster_by.length > 0 || l.notes.length > 0 || l.requires_partition_filter) && (
          <section className="space-y-1">
            <h3 className="text-[11px] font-medium uppercase tracking-wide text-muted">Partitioning and clustering</h3>
            {l.partition_by.length > 0 && (
              <p>
                Partitioned{l.partition_kind ? ` by ${l.partition_kind}` : ""} on <span className="font-mono">{l.partition_by.join(", ")}</span>
                {l.requires_partition_filter && <span className="text-warning"> · queries must filter on it</span>}
              </p>
            )}
            {l.cluster_by.length > 0 && (
              <p>
                Clustered by <span className="font-mono">{l.cluster_by.join(", ")}</span>
              </p>
            )}
            {l.notes.map((n) => (
              <p key={n} className="text-muted">
                {n}
              </p>
            ))}
          </section>
        )}

        <section className="space-y-1.5">
          <h3 className="text-[11px] font-medium uppercase tracking-wide text-muted">Checks and triggers {checks.ran.status === "done" || trig.ran.status === "done" ? `(${constraints.length})` : ""}</h3>
          {checksWhy && trigWhy && checksWhy === trigWhy ? (
            <p className="text-muted">Not available: {checksWhy}.</p>
          ) : (
            <>
              {checks.ran.status === "running" || trig.ran.status === "running" ? <Busy what="Reading the checks and triggers…" /> : null}
              {checksWhy && <p className="text-muted">Checks: {checksWhy}.</p>}
              {trigWhy && <p className="text-muted">Triggers: {trigWhy}.</p>}
              {checks.ran.status === "error" && <p className="text-danger">The checks couldn't be read: {checks.ran.message}</p>}
              {trig.ran.status === "error" && <p className="text-danger">The triggers couldn't be read: {trig.ran.message}</p>}
              {constraints.length === 0 && (checks.ran.status === "done" || trig.ran.status === "done") && checks.ran.status !== "error" && trig.ran.status !== "error" && <p className="text-muted">None.</p>}
              {constraints.length > 0 && (
                <ul className="space-y-1">
                  {constraints.map((c, i) => (
                    <li key={`${c.what}:${c.name}:${i}`} className="rounded border border-line/60 px-2 py-1">
                      <div className="flex items-center gap-2 text-[11px] text-muted">
                        <span className="rounded bg-hover px-1 uppercase tracking-wide">{c.what}</span>
                        {c.name && <span className="font-mono">{c.name}</span>}
                      </div>
                      <div className="mt-0.5 whitespace-pre-wrap break-words font-mono text-[11.5px]">{c.definition}</div>
                    </li>
                  ))}
                </ul>
              )}
            </>
          )}
        </section>
      </div>
      {checks.ran.status !== "idle" && <RanSql e={e} sql={checks.ran.sql} ms={checks.ran.status === "done" ? checks.ran.rows.duration_ms : undefined} />}
      {trig.ran.status !== "idle" && <RanSql e={e} sql={trig.ran.sql} ms={trig.ran.status === "done" ? trig.ran.rows.duration_ms : undefined} />}
    </div>
  );
}

/** A Delta history row's figures, by column name (any case). */
const field = (cols: string[], row: (string | null)[], name: string) => {
  const i = cols.findIndex((c) => c.toLowerCase() === name.toLowerCase());
  return i < 0 ? null : (row[i] ?? null);
};

/** Delta versions (Databricks), or the rows as of a time (Snowflake, BigQuery). */
export function HistoryPart({ explore: e }: { explore: Explore }) {
  const { conn, reference } = e;
  const kind = conn.config.kind;
  const how = historyKind(kind);
  const { local } = localFrom(e);
  const hist = useExploreRead(e, "history");
  const built = historySql(kind, reference.schema, reference.name);
  const [time, setTime] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  useOnce(() => {
    if (how === "versions" && "sql" in built && !local && reference.kind !== "view") void hist.read(built.sql, null);
  });
  const open = (title: string, b: ReturnType<typeof asOfSql>) => {
    if (!("sql" in b)) return setProblem(b.unavailable);
    setProblem(null);
    useStore.getState().newTab({ title, sql: b.sql, connection_id: conn.id });
  };
  if (!how || local || reference.kind === "view")
    return (
      <p className="p-3 text-[12px] text-muted">
        Not available{reference.kind === "view" ? " for a view" : ` on ${traitsOf(kind).name}`}: {reference.kind === "view" ? "a view keeps no rows of its own" : "it doesn't keep earlier versions of a table's rows"}. Delta tables (Databricks) list their versions; Snowflake and BigQuery read a table as it was at a time (time travel).
      </p>
    );
  const cols = hist.ran.status === "done" ? hist.ran.rows.columns : [];
  return (
    <div className="flex h-full min-h-0 flex-col text-[12px]">
      <div className="min-h-0 flex-1 space-y-3 overflow-auto p-3">
        <form
          className="flex flex-wrap items-center gap-2"
          onSubmit={(ev) => {
            ev.preventDefault();
            open(`${reference.name} @ ${time.trim()}`, asOfSql(kind, reference.schema, reference.name, { time }));
          }}
        >
          <Clock size={12} className="text-muted" />
          <span className="text-muted">The rows as they were at</span>
          <input
            aria-label="As of"
            className="w-44 rounded border border-line bg-bg px-2 py-0.5 font-mono text-[11.5px]"
            placeholder="2026-11-20 08:30"
            value={time}
            onChange={(ev) => setTime(ev.target.value)}
          />
          <button type="submit" className="btn-ghost border border-line py-0.5 text-[12px]" disabled={!AS_OF_TIME.test(time.trim())} title="A query tab with the SELECT; it runs when you run it">
            <FileCode2 size={12} /> Open in editor
          </button>
        </form>
        <p className="text-muted">
          {kind === "snowflake"
            ? "Within the table's time travel retention (1 day by default; the Statistics part shows it), in the session's time zone."
            : kind === "bigquery"
              ? "Within the dataset's time travel window (7 days by default), in UTC. BigQuery bills the columns the query scans, whatever its LIMIT: the editor shows the bytes before it runs."
              : "Within the table's Delta retention (log 30 days, data files 7 days by default), in the session's time zone (UTC unless set)."}
        </p>
        {problem && (
          <p className="text-danger" role="alert">
            {problem}
          </p>
        )}
        {how === "versions" && (
          <section className="space-y-1.5">
            <h3 className="flex items-center gap-2 text-[11px] font-medium uppercase tracking-wide text-muted">
              Versions {hist.ran.status === "done" ? `(the latest ${hist.ran.rows.rows.length})` : ""}
              <button className="btn-ghost ml-auto py-0 text-[11px] normal-case tracking-normal" disabled={hist.ran.status === "running"} onClick={() => "sql" in built && void hist.read(built.sql, null)}>
                <RefreshCw size={11} /> Refresh
              </button>
            </h3>
            {hist.ran.status === "idle" || hist.ran.status === "running" ? (
              <Busy what="Reading the table's history…" />
            ) : hist.ran.status === "error" ? (
              <p className="text-danger">The history couldn't be read: {hist.ran.message}</p>
            ) : hist.ran.rows.rows.length === 0 ? (
              <p className="text-muted">No versions recorded.</p>
            ) : (
              <table className="w-full border-collapse">
                <thead className="text-left text-[11px] uppercase tracking-wide text-muted">
                  <tr>
                    <th className="px-2 py-1 text-right font-medium">Version</th>
                    <th className="px-2 py-1 font-medium">When</th>
                    <th className="px-2 py-1 font-medium">Operation</th>
                    <th className="px-2 py-1 font-medium">By</th>
                    <th className="px-2 py-1 text-right font-medium" title="Rows the operation wrote (for MERGE and UPDATE, unchanged rows rewritten into new files count too)">Rows written</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {hist.ran.rows.rows.map((r, i) => {
                    const version = num(field(cols, r, "version"));
                    const written = rowsWritten(field(cols, r, "operationMetrics"));
                    return (
                      <tr key={i} className="border-t border-line/60">
                        <td className="px-2 py-1 text-right tabular-nums">{version ?? ""}</td>
                        <td className="px-2 py-1 font-mono text-[11px]">{field(cols, r, "timestamp")}</td>
                        <td className="px-2 py-1">{field(cols, r, "operation")}</td>
                        <td className="max-w-[200px] truncate px-2 py-1 text-muted">{field(cols, r, "userName") ?? ""}</td>
                        <td className="px-2 py-1 text-right tabular-nums">{written != null ? formatCount(written) : ""}</td>
                        <td className="px-2 py-1 text-right">
                          {version != null && (
                            <button className="btn-ghost py-0 text-[11px]" onClick={() => open(`${reference.name} v${version}`, asOfSql(kind, reference.schema, reference.name, { version }))} title="A query tab reading this version">
                              <FileCode2 size={11} /> Query this version
                            </button>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            )}
          </section>
        )}
      </div>
      {hist.ran.status !== "idle" && <RanSql e={e} sql={hist.ran.sql} ms={hist.ran.status === "done" ? hist.ran.rows.duration_ms : undefined} />}
    </div>
  );
}
