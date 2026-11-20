// Table explorer: Partitions (each partition with its bound and size, as
// DBeaver lists them apart from the DDL) and Permissions (who may do what on
// the table). Catalog reads, read when the part opens (BigQuery's partitions,
// which it bills, on demand); each shows the SQL it ran.
import { useEffect, useRef, useState } from "react";
import { Loader2, RefreshCw } from "lucide-react";
import { api } from "../lib/api";
import { billsScans, exploreKey, num } from "../lib/explore";
import { COLUMN_LABELS, GRANT_COLUMNS, notPartitioned, partitionsSql, PARTITIONS_MAX, permissionsSql, unpartitioned } from "../lib/exploreAccess";
import { traitsOf } from "../lib/sourceTraits";
import type { ExploreRows } from "../lib/types";
import { formatBytes, formatCount } from "../lib/util";
import { isRedshift } from "./connection/parts";
import { ask, localFrom, RanSql, useExploreRead, type Explore } from "./tableExplore";

const Busy = ({ what }: { what: string }) => (
  <p className="flex items-center gap-1.5 text-muted">
    <Loader2 size={12} className="animate-spin" /> {what}
  </p>
);

/** A Postgres connection that is Redshift (its flavor, or a Redshift host). */
const redshiftOf = (e: Explore) => e.conn.config.kind === "postgres" && isRedshift({ ...(e.conn.config.options ?? {}), host: e.conn.config.host ?? undefined });

/**
 * Rows as a table: the columns `keep` names (in order) when given, else
 * all; sizes and counts formatted unless `raw` (Databricks partitions: the
 * table's own partition columns, whatever their names).
 */
function CatalogTable({ rows, keep, raw }: { rows: ExploreRows; keep?: string[]; raw?: boolean }) {
  const lower = rows.columns.map((c) => c.toLowerCase());
  const shown = (keep ? keep.map((k) => lower.indexOf(k)).filter((i) => i >= 0) : lower.map((_, i) => i)).filter((i) => COLUMN_LABELS[lower[i]] !== "" || rows.rows.some((r) => r[i]));
  const cols = shown.length ? shown : lower.map((_, i) => i);
  const label = (i: number) => (raw ? rows.columns[i] : (COLUMN_LABELS[lower[i]] ?? rows.columns[i]));
  const right = (c: string) => !raw && (c === "row_count" || c === "bytes" || c === "parts" || c === "position" || c === "subpartitions");
  const text = (c: string, v: string | null) => {
    if (v == null) return "";
    if (raw) return v;
    const n = num(v);
    if (c === "bytes" && n != null) return formatBytes(n);
    if ((c === "row_count" || c === "parts") && n != null) return n < 0 ? "unknown" : formatCount(n);
    return v;
  };
  return (
    <table className="w-full border-collapse">
      <thead className="sticky top-0 bg-panel text-left text-[11px] uppercase tracking-wide text-muted">
        <tr>
          {cols.map((i) => (
            <th key={i} className={`px-2 py-1 font-medium ${right(lower[i]) ? "text-right" : ""}`}>
              {label(i)}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.rows.map((r, j) => (
          <tr key={j} className="border-t border-line/60 hover:bg-hover/40">
            {cols.map((i) => (
              <td
                key={i}
                className={`max-w-[360px] truncate px-2 py-1 ${right(lower[i]) ? "text-right tabular-nums" : ""} ${lower[i] === "bound" || lower[i] === "name" ? "font-mono text-[11.5px]" : ""} ${r[i] === "DENY" || r[i] === "revoked" ? "text-danger" : ""}`}
                title={r[i] ?? undefined}
              >
                {text(lower[i], r[i])}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** The table's partitions. */
export function PartitionsPart({ explore: e }: { explore: Explore }) {
  const { conn, reference } = e;
  const kind = conn.config.kind;
  const { local } = localFrom(e);
  const built = partitionsSql(kind, reference.schema, reference.name, reference.kind, local, redshiftOf(e));
  const part = useExploreRead(e, "partitions");
  const billed = billsScans(kind);
  // BigQuery's question is open (Read partitions doesn't open a second).
  const [asking, setAsking] = useState(false);
  const load = async () => {
    if (!("sql" in built) || asking) return;
    if (billed) {
      setAsking(true);
      // BigQuery bills INFORMATION_SCHEMA reads (10 MB at least): asked at each read.
      const est = await api.estimateCost(conn.id, exploreKey(e.tabId, "partitions"), built.sql).catch(() => null);
      const reasons = [est?.bytes != null ? `BigQuery bills this metadata read: about ${formatBytes(Math.max(est.bytes, 10 * 1024 * 1024))} (10 MB at least).` : "BigQuery bills this metadata read (10 MB at least); the estimate couldn't be read."];
      const ok = await ask(`Read the partitions of ${reference.name}?`, reasons, "Read them");
      setAsking(false);
      if (!ok) return;
    }
    void part.read(built.sql, null);
  };
  const started = useRef(false);
  useEffect(() => {
    if (started.current || billed) return;
    started.current = true;
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once, when the part opens; Refresh reads again
  }, []);
  if (!("sql" in built)) return <p className="p-3 text-[12px] text-muted">Partitions: not available: {built.unavailable}.</p>;
  const ran = part.ran;
  const none = (ran.status === "done" && unpartitioned(kind, ran.rows)) || (ran.status === "error" && kind === "databricks" && notPartitioned(ran.message));
  return (
    <div className="flex h-full min-h-0 flex-col text-[12px]">
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line px-2 py-1.5">
        <button className="btn-ghost border border-line py-0.5 text-[12px]" onClick={() => void load()} disabled={ran.status === "running" || asking} title={billed ? "Read the partitions (BigQuery bills it)" : "Read the partitions again"}>
          <RefreshCw size={12} className={ran.status === "running" ? "animate-spin" : ""} /> {billed && ran.status === "idle" ? "Read partitions" : "Refresh"}
        </button>
        {ran.status === "done" && !none && (
          <span className="text-muted">
            {ran.rows.rows.length >= PARTITIONS_MAX ? `The first ${formatCount(PARTITIONS_MAX)} partitions (there may be more)` : `${formatCount(ran.rows.rows.length)} partition${ran.rows.rows.length === 1 ? "" : "s"}`}
          </span>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-3">
        {ran.status === "idle" ? (
          billed ? <p className="text-muted">Read partitions lists them from INFORMATION_SCHEMA.PARTITIONS, which BigQuery bills (10 MB at least): it asks first.</p> : <Busy what="Reading the partitions…" />
        ) : ran.status === "running" ? (
          <Busy what="Reading the partitions…" />
        ) : none ? (
          <p className="text-muted">{kind === "mssql" && ran.status === "done" && ran.rows.rows.length === 1 ? `Not partitioned: one partition of ${formatCount(num(ran.rows.rows[0][2]) ?? 0)} rows.` : "Not partitioned."}</p>
        ) : ran.status === "error" ? (
          <p className="text-danger" role="alert">
            The partitions couldn't be read: {ran.message}
          </p>
        ) : (
          <CatalogTable rows={ran.rows} raw={kind === "databricks"} />
        )}
      </div>
      {ran.status !== "idle" && <RanSql e={e} sql={ran.sql} ms={ran.status === "done" ? ran.rows.duration_ms : undefined} />}
    </div>
  );
}

/** Who may do what on the table. */
export function PermissionsPart({ explore: e }: { explore: Explore }) {
  const { conn, reference } = e;
  const kind = conn.config.kind;
  const { local } = localFrom(e);
  const built = permissionsSql(kind, reference.schema, reference.name, reference.kind, local, redshiftOf(e));
  const perm = useExploreRead(e, "permissions");
  const load = () => "sql" in built && void perm.read(built.sql, null);
  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once, when the part opens; Refresh reads again
  }, []);
  if (!("sql" in built)) return <p className="p-3 text-[12px] text-muted">Permissions: not available: {built.unavailable}.</p>;
  const ran = perm.ran;
  return (
    <div className="flex h-full min-h-0 flex-col text-[12px]">
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line px-2 py-1.5">
        <button className="btn-ghost border border-line py-0.5 text-[12px]" onClick={load} disabled={ran.status === "running"} title="Read the permissions again">
          <RefreshCw size={12} className={ran.status === "running" ? "animate-spin" : ""} /> Refresh
        </button>
        <span className="text-muted">As {traitsOf(kind).name} shows them to this connection's user: a catalog may list only what the user may see (on some, only its own grants).</span>
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-3">
        {ran.status === "idle" || ran.status === "running" ? (
          <Busy what="Reading the permissions…" />
        ) : ran.status === "error" ? (
          <p className="text-danger" role="alert">
            The permissions couldn't be read: {ran.message}
          </p>
        ) : ran.rows.rows.length === 0 ? (
          <p className="text-muted">No grants visible to this user.</p>
        ) : (
          <CatalogTable rows={ran.rows} keep={GRANT_COLUMNS[kind]} />
        )}
      </div>
      {ran.status !== "idle" && <RanSql e={e} sql={ran.sql} ms={ran.status === "done" ? ran.rows.duration_ms : undefined} />}
    </div>
  );
}
