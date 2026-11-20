// Table explorer: one tab per table or view that gathers what DataBrain
// knows and can read about it: columns with their profile, sample rows,
// statistics, relations, usage, indexes, partitions, permissions,
// history, DDL and notes (see
// docs/TODO.md "Table explorer" for what each part leaves out).
import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { AlertTriangle, BarChart3, Copy, FileCode2, Hash, Key, Link2, Loader2, NotebookPen, Play, RefreshCw, Shuffle, Square, Table2 } from "lucide-react";
import { api, toError } from "../lib/api";
import { RESULTS_SCHEMA_ID, tablePath } from "../lib/catalog";
import { traitsOf } from "../lib/sourceTraits";
import type { ConnectionView, ObjectDetail, TableNote, TableNotes } from "../lib/types";
import { approxDistinct, billsScans, columnClass, countSql, num, profileSql, PROFILE_MAX_COLUMNS, readProfile, STAT_LABELS, statisticsSql, topValuesSql, TOP_VALUES, type Built } from "../lib/explore";
import { formatBytes, formatCount, relativeTime, sampleSql, shortCount } from "../lib/util";
import { useStore, type DdlRef } from "../store";
import { DdlTab } from "./DdlTab";
import { RelationsPart, UsagePart } from "./TableRelations";
import { HistoryPart, IndexesPart } from "./TableHistory";
import { PartitionsPart, PermissionsPart } from "./TableAccess";
import { localFrom, RanSql, useExploreRead, type Explore } from "./tableExplore";
import { ResultsPanel } from "./ResultsPanel";
import { ConnDot, EnvBadge } from "./ui";
import { objectRef, selectTop } from "./CatalogSearch";

type Part = "columns" | "data" | "stats" | "relations" | "usage" | "indexes" | "partitions" | "permissions" | "history" | "ddl" | "notes";
type Detail = { status: "loading" } | { status: "ready"; detail: ObjectDetail } | { status: "error"; message: string };
type Notes = { status: "loading" } | { status: "ready"; notes: TableNotes } | { status: "error"; message: string };

export function TableTab({ tabId, reference, visible }: { tabId: string; reference: DdlRef; visible: boolean }) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const conn = useStore((s) => s.connections.find((c) => c.id === tab?.connection_id));
  // Columns first: what the table is, read from the catalog. Data reads
  // rows when it is opened.
  const [part, setPart] = useState<Part>("columns");
  // Parts opened so far stay mounted (hidden): Data keeps its rows, settings
  // and SQL, the DDL isn't read again.
  const [opened, setOpened] = useState<ReadonlySet<Part>>(new Set(["columns"]));
  const [detail, setDetail] = useState<Detail>({ status: "loading" });
  const [notes, setNotes] = useState<Notes>({ status: "loading" });
  const [detailSeq, setDetailSeq] = useState(0);
  const [notesSeq, setNotesSeq] = useState(0);
  // Reads that scan the whole table (profile, count, top values) ask once per tab on production or a large table.
  const heavyAsked = useRef(false);
  const kind = conn?.config.kind;
  const path = kind ? tablePath(kind, reference.schema, reference.name) : reference.name;
  const connId = conn?.id;

  useEffect(() => {
    if (!connId) return setDetail({ status: "error", message: "The connection of this tab no longer exists." });
    let live = true;
    setDetail({ status: "loading" });
    api
      .describeObject(connId, reference.schema, reference.name)
      .then((d) => live && setDetail({ status: "ready", detail: d }))
      .catch((e) => live && setDetail({ status: "error", message: toError(e).message }));
    return () => {
      live = false;
    };
  }, [connId, reference.schema, reference.name, detailSeq]);
  useEffect(() => {
    if (!connId) return;
    let live = true;
    api
      .knTableNotes(connId, reference.schema, reference.name)
      .then((n) => live && setNotes({ status: "ready", notes: n }))
      .catch((e) => live && setNotes({ status: "error", message: toError(e).message }));
    return () => {
      live = false;
    };
  }, [connId, reference.schema, reference.name, notesSeq]);

  if (!conn || !kind) return <div className="p-4 text-[12.5px] text-muted" style={{ display: visible ? "block" : "none" }}>The connection of this tab no longer exists.</div>;
  const d = detail.status === "ready" ? detail.detail : null;
  const object = d?.object;
  const columns = d?.columns ?? [];
  const loadable = reference.kind === "table" && !!traitsOf(kind).load;
  const noteList = notes.status === "ready" ? notes.notes.notes : [];
  const explore: Explore = { tabId, conn, reference, rowEstimate: object?.row_estimate ?? null, asked: heavyAsked };
  const parts: { key: Part; label: string; count?: number }[] = [
    { key: "columns", label: "Columns", count: d ? columns.length : undefined },
    { key: "data", label: "Data" },
    { key: "stats", label: "Statistics" },
    { key: "relations", label: "Relations" },
    { key: "usage", label: "Usage" },
    { key: "indexes", label: "Indexes" },
    { key: "partitions", label: "Partitions" },
    { key: "permissions", label: "Permissions" },
    { key: "history", label: "History" },
    { key: "ddl", label: "DDL" },
    { key: "notes", label: "Notes", count: noteList.length || undefined },
  ];
  const show = (p: Part) => {
    setPart(p);
    setOpened((o) => (o.has(p) ? o : new Set(o).add(p)));
  };
  const slot = (p: Part, body: ReactNode) =>
    opened.has(p) && (
      <div key={p} className="h-full min-h-0" style={{ display: part === p ? "block" : "none" }}>
        {body}
      </div>
    );

  return (
    <div className="h-full min-h-0 flex-col" style={{ display: visible ? "flex" : "none" }} aria-label={`Table ${path}`}>
      <header className="shrink-0 space-y-1.5 border-b border-line px-3 py-2">
        <div className="flex min-w-0 items-center gap-2 text-[12.5px]">
          <Table2 size={15} className="shrink-0 text-accent" />
          <span className="min-w-0 truncate font-mono font-semibold" title={path}>
            {path}
          </span>
          <span className="shrink-0 text-muted">{reference.kind.replace("_", " ")}</span>
          <span className="flex shrink-0 items-center gap-1 text-muted">
            · <ConnDot color={conn.color} connected={conn.connected} /> {conn.name}
          </span>
          {conn.env !== "none" && <EnvBadge env={conn.env} />}
          {object?.row_estimate != null && object.row_estimate >= 0 && (
            <span className="shrink-0 text-muted" title={`About ${formatCount(object.row_estimate)} rows (the database's estimate)`}>
              · ~{shortCount(object.row_estimate)} rows
            </span>
          )}
          <button
            className="icon-btn ml-auto h-6 w-6 shrink-0"
            onClick={() => {
              setDetailSeq((n) => n + 1);
              setNotesSeq((n) => n + 1);
            }}
            title="Read the table's description and notes again"
            aria-label="Refresh"
          >
            <RefreshCw size={12} className={detail.status === "loading" ? "animate-spin" : ""} />
          </button>
        </div>
        {object?.comment?.trim() && <Description text={object.comment.trim()} />}
        <div className="flex flex-wrap items-center gap-1">
          <button className="btn-ghost border border-line py-0.5 text-[12px]" onClick={() => selectTop(conn, { schema: reference.schema, name: reference.name, kind: reference.kind })} title="A new query tab with its first 100 rows">
            <Play size={12} /> Query
          </button>
          {loadable && (
            <button
              className="btn-ghost border border-line py-0.5 text-[12px]"
              onClick={() => useStore.getState().openTestDataDialog({ connection_id: conn.id, schema: reference.schema, table: reference.name })}
              title="Generate test data from its column types"
            >
              <Shuffle size={12} /> Test data…
            </button>
          )}
        </div>
      </header>
      <nav className="flex shrink-0 items-center gap-1 border-b border-line px-2" role="tablist" aria-label="Table parts">
        {parts.map((p) => (
          <button
            key={p.key}
            role="tab"
            aria-selected={part === p.key}
            onClick={() => show(p.key)}
            className={`-mb-px border-b-2 px-2 py-1.5 text-[12px] ${part === p.key ? "border-accent text-fg" : "border-transparent text-muted hover:text-fg"}`}
          >
            {p.label}
            {p.count != null && <span className="ml-1 text-[10.5px] text-muted">{p.count}</span>}
          </button>
        ))}
      </nav>
      <div className="min-h-0 flex-1">
        {slot("data", <DataPart tabId={tabId} conn={conn} reference={reference} />)}
        {slot(
          "columns",
          detail.status === "loading" ? (
            <Busy />
          ) : detail.status === "error" ? (
            <p className="p-3 text-[12px] text-danger">{detail.message}</p>
          ) : (
            <ColumnsPart detail={detail.detail} notes={noteList} explore={explore} />
          ),
        )}
        {slot("stats", <StatisticsPart explore={explore} />)}
        {slot("relations", <RelationsPart conn={conn} reference={reference} detail={d} />)}
        {slot("usage", <UsagePart conn={conn} reference={reference} />)}
        {slot("indexes", <IndexesPart explore={explore} />)}
        {slot("partitions", <PartitionsPart explore={explore} />)}
        {slot("permissions", <PermissionsPart explore={explore} />)}
        {slot("history", <HistoryPart explore={explore} />)}
        {/* The DDL view as Show DDL opens it, in this tab (read once). */}
        {slot("ddl", <DdlTab tabId={tabId} reference={reference} visible={visible && part === "ddl"} />)}
        {slot("notes", <NotesPart conn={conn} notes={notes} onSaved={() => setNotesSeq((n) => n + 1)} />)}
      </div>
    </div>
  );
}

/** The table's description: up to three lines, the rest scrolls; selectable, with Copy. */
function Description({ text }: { text: string }) {
  return (
    <div className="group flex items-start gap-1">
      <div className="max-h-[4.5em] min-w-0 flex-1 select-text overflow-y-auto whitespace-pre-wrap break-words text-[11.5px] leading-[1.5em] text-muted" aria-label="Description">
        {text}
      </div>
      <button
        className="icon-btn h-5 w-5 shrink-0 opacity-60 group-hover:opacity-100"
        onClick={() =>
          void writeText(text)
            .then(() => useStore.getState().toast("Description copied", "success"))
            .catch(() => useStore.getState().toast("Couldn't copy the description", "error"))
        }
        title="Copy the description"
        aria-label="Copy the description"
      >
        <Copy size={11} />
      </button>
    </div>
  );
}

function Busy() {
  return (
    <div className="flex items-center gap-1.5 p-3 text-[12px] text-muted">
      <Loader2 size={12} className="animate-spin" /> Reading the table…
    </div>
  );
}

const SAMPLE_SIZES = [100, 1000, 10000];

/** Sample rows: the first N, filtered by a condition, in the result grid (sort, find, copy, export, load into). */
function DataPart({ tabId, conn, reference }: { tabId: string; conn: ConnectionView; reference: DdlRef }) {
  const kind = conn.config.kind;
  const filterable = traitsOf(kind).preview !== "mongo_find" && traitsOf(kind).preview !== "cypher";
  const [n, setN] = useState(100);
  const [filter, setFilter] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  // A DuckDB result or attached file is named as the Query button names it.
  const from =
    kind === "duckdb" && (reference.schema.endsWith(".files") || reference.schema === RESULTS_SCHEMA_ID)
      ? objectRef(conn, { schema: reference.schema, name: reference.name, kind: reference.kind })
      : undefined;
  const sql = useMemo(() => sampleSql(kind, reference.schema, reference.name, n, filter, from), [kind, reference.schema, reference.name, n, filter, from]);
  const run = useStore((s) => s.runs[tabId]);
  // The SQL whose rows the grid shows ("Open in editor" takes it).
  const [ran, setRan] = useState<string | null>(null);
  // A production connection asks once per tab before the first sample.
  const prod = conn.env === "prod";
  const prodAsked = useRef(false);
  const ask = (title: string, reasons: string[]) =>
    new Promise<boolean>((resolve) =>
      useStore.getState().askConfirm({ title, reasons, confirmLabel: "Read rows", onConfirm: () => resolve(true), onCancel: () => resolve(false) }),
    );
  const go = async (text = sql) => {
    setProblem(null);
    setChecking(true);
    try {
      // One read, no parameters: the condition typed in can't start a write
      // (the engine refuses it again for the explorer's runs).
      await api.checkSampleSql(conn.id, text);
      if (billsScans(kind)) {
        const est = await api.estimateCost(conn.id, tabId, text).catch(() => null);
        const reasons = [
          est?.bytes != null
            ? `This query reads about ${formatBytes(est.bytes)}: BigQuery bills the columns it scans, and LIMIT doesn't lower it.`
            : "BigQuery bills the columns this query scans, and LIMIT doesn't lower it; the estimate couldn't be read.",
        ];
        if (est?.over_limit != null) reasons.push(`That is over the connection's limit of ${formatBytes(est.over_limit)} billed: BigQuery will refuse it.`);
        if (prod) reasons.push(`"${conn.name}" is a production connection.`);
        if (!(await ask(`Read sample rows of ${reference.name}?`, reasons))) return;
        prodAsked.current = true;
      } else if (prod && !prodAsked.current) {
        const ok = await ask(`Read sample rows of ${reference.name}?`, [`"${conn.name}" is a production connection: this reads up to ${formatCount(n)} rows of it${reference.kind === "table" ? "" : " (a view runs its query)"}. This tab won't ask again.`]);
        if (!ok) return;
        prodAsked.current = true;
      }
      setRan(text);
      void useStore.getState().runSql(tabId, conn.id, text, 0, undefined, undefined, undefined, n, undefined, "explorer");
    } catch (e) {
      setProblem(toError(e).message);
    } finally {
      setChecking(false);
    }
  };
  // The first rows as soon as the tab opens, where that costs nothing more
  // than the rows and needn't ask (once: React runs effects twice in
  // development, and the run isn't in the store yet; Run reads them again).
  const waits = billsScans(kind) || prod;
  const started = useRef(false);
  useEffect(() => {
    if (started.current || run || waits) return;
    started.current = true;
    void go();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once, when the tab opens; Run reads the rows again
  }, []);
  return (
    <div className="flex h-full min-h-0 flex-col">
      <form
        className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line px-2 py-1.5 text-[12px]"
        onSubmit={(e) => {
          e.preventDefault();
          void go();
        }}
      >
        <label className="flex items-center gap-1 text-muted">
          First
          <select aria-label="Rows" className="rounded border border-line bg-bg px-1 py-0.5" value={n} onChange={(e) => setN(Number(e.target.value))}>
            {SAMPLE_SIZES.map((s) => (
              <option key={s} value={s}>
                {formatCount(s)}
              </option>
            ))}
          </select>
          rows
        </label>
        {!filterable && (
          <span className="text-muted" title={`A where condition is SQL, and ${traitsOf(kind).name} has its own query language`}>
            No condition on {traitsOf(kind).name}: Open in editor to filter
          </span>
        )}
        {filterable && (
          <input
            aria-label="Where condition"
            className="min-w-[200px] flex-1 rounded border border-line bg-bg px-2 py-0.5 font-mono text-[11.5px]"
            placeholder="where …  e.g. status = 'paid'"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        )}
        <button type="submit" className="btn-primary py-0.5 text-[12px]" disabled={run?.running || checking}>
          {run?.running || checking ? <Loader2 size={12} className="animate-spin" /> : <Play size={12} />} Run
        </button>
        <button
          type="button"
          className="btn-ghost py-0.5 text-[12px]"
          onClick={() => useStore.getState().newTab({ title: reference.name, sql: ran ?? sql, connection_id: conn.id })}
          title={ran ? "A query tab with the SQL of the rows shown" : "A query tab with this SQL"}
        >
          <FileCode2 size={12} /> Open in editor
        </button>
      </form>
      {problem && (
        <div className="flex shrink-0 items-start gap-1 border-b border-line bg-danger/5 px-2 py-1 text-[11.5px] text-danger" role="alert">
          <AlertTriangle size={12} className="mt-0.5 shrink-0" /> {problem}
        </div>
      )}
      <div className="min-h-0 flex-1">
        {!run && waits ? (
          <p className="p-3 text-[12px] text-muted">
            {billsScans(kind)
              ? "Press Run to read sample rows: BigQuery bills the columns a query scans, whatever its LIMIT, so they aren't read on opening."
              : `Press Run to read sample rows: "${conn.name}" is a production connection, so they aren't read on opening.`}
          </p>
        ) : (
          <ResultsPanel tabId={tabId} connectionId={conn.id} title={reference.name} noEditor />
        )}
      </div>
    </div>
  );
}

/** The columns as the database describes them, with keys, comments and notes. */
function ColumnsPart({ detail, notes, explore }: { detail: ObjectDetail; notes: TableNote[]; explore: Explore }) {
  const [profile, setProfile] = useState<ReturnType<typeof readProfile> | null>(null);
  // The column whose top values show under it (one at a time).
  const [open, setOpen] = useState<string | null>(null);
  const kind = explore.conn.config.kind;
  const approx = approxDistinct(kind);
  const pct = (n: number | null) => (n == null || !profile?.total ? "" : `${((n / profile.total) * 100).toFixed(n === profile.total || n === 0 ? 0 : 1)}%`);
  const fkOf = (col: string) => detail.foreign_keys.find((f) => f.columns.length === 1 && f.columns[0].toLowerCase() === col.toLowerCase());
  // The server says which column a note is on (a note on the table is on none).
  // An approved note before a proposed one.
  const noteOf = (col: string) => {
    const on = notes.filter((n) => n.column?.toLowerCase() === col.toLowerCase());
    return on.find((n) => n.status !== "proposed") ?? on[0];
  };
  if (!detail.columns.length) return <p className="p-3 text-[12px] text-muted">The database lists no columns for it.</p>;
  return (
    <div className="flex h-full min-h-0 flex-col">
      <ProfileBar e={explore} detail={detail} onProfile={(p) => p && setProfile(p)} />
      <div className="min-h-0 flex-1 overflow-auto">
      {profile?.total != null && <div className="px-2 pt-1.5 text-[11.5px] text-muted">{formatCount(profile.total)} rows read</div>}
      <table className="w-full border-collapse text-[12px]">
        <thead className="sticky top-0 bg-panel text-left text-[11px] uppercase tracking-wide text-muted">
          <tr>
            <th className="w-8 px-2 py-1 text-right font-medium">#</th>
            <th className="px-2 py-1 font-medium">Column</th>
            <th className="px-2 py-1 font-medium">Type</th>
            <th className="px-2 py-1 font-medium">Null</th>
            <th className="px-2 py-1 font-medium">Key</th>
            <th className="px-2 py-1 font-medium">Default</th>
            <th className="px-2 py-1 font-medium">Comment / note</th>
            {profile && (
              <>
                <th className="px-2 py-1 text-right font-medium">Non-null</th>
                <th className="px-2 py-1 text-right font-medium" title={approx ? "Approximate: the database's fast distinct count" : undefined}>
                  Distinct{approx ? " ≈" : ""}
                </th>
                <th className="px-2 py-1 font-medium">Min … max</th>
              </>
            )}
          </tr>
        </thead>
        <tbody>
          {detail.columns.map((c, i) => {
            const fk = fkOf(c.name);
            const note = noteOf(c.name);
            const p = profile?.columns[i];
            const countable = columnClass(kind, c.data_type) !== "other";
            return (
              <Fragment key={c.name}>
              <tr className="border-t border-line/60 hover:bg-hover/40">
                <td className="px-2 py-1 text-right tabular-nums text-muted">{i + 1}</td>
                <td className="px-2 py-1 font-mono">{c.name}</td>
                <td className="px-2 py-1 font-mono text-muted">{c.data_type}</td>
                <td className="px-2 py-1 text-muted">{c.nullable ? "yes" : "no"}</td>
                <td className="px-2 py-1">
                  <span className="flex items-center gap-1">
                    {c.is_primary_key && (
                      <span className="flex items-center gap-0.5 text-warning" title="Primary key">
                        <Key size={11} /> PK
                      </span>
                    )}
                    {fk && (
                      <span className="flex items-center gap-0.5 text-accent" title={`Foreign key to ${fk.ref_schema}.${fk.ref_table} (${fk.ref_columns.join(", ")})`}>
                        <Link2 size={11} /> {fk.ref_table}.{fk.ref_columns[0]}
                      </span>
                    )}
                  </span>
                </td>
                <td className="max-w-[160px] truncate px-2 py-1 font-mono text-[11px] text-muted" title={c.default ?? undefined}>
                  {c.default ?? ""}
                </td>
                <td className="max-w-[320px] px-2 py-1 text-[11.5px]">
                  {c.comment && <div className="truncate text-muted" title={c.comment}>{c.comment}</div>}
                  {note && (
                    <div className={`flex items-start gap-1 truncate ${note.status === "proposed" ? "text-warning" : "text-accent"}`} title={note.status === "proposed" ? `Proposed, to review in Knowledge: ${note.body}` : note.body}>
                      <NotebookPen size={10} className="mt-0.5 shrink-0" /> {note.status === "proposed" && <span className="shrink-0">(proposed)</span>} {note.body}
                    </div>
                  )}
                </td>
                {profile && (
                  <>
                    <td className="px-2 py-1 text-right tabular-nums" title={p?.nonNull != null ? `${formatCount(p.nonNull)} non-null` : "Past the profiled columns"}>
                      {pct(p?.nonNull ?? null)}
                    </td>
                    <td className="px-2 py-1 text-right tabular-nums">
                      {p?.distinct != null ? (
                        <button className="underline decoration-dotted hover:text-accent" onClick={() => setOpen(open === c.name ? null : c.name)} title="Show its most common values" aria-expanded={open === c.name}>
                          {formatCount(p.distinct)}
                        </button>
                      ) : (
                        <span className="text-muted" title={countable ? "Past the profiled columns" : "Not counted: values of this type can't be compared on every database"}>
                          —
                        </span>
                      )}
                    </td>
                    <td className="max-w-[260px] truncate px-2 py-1 font-mono text-[11px]" title={p?.lo != null ? `${p.lo} … ${p.hi}` : undefined}>
                      {p?.lo != null ? `${p.lo} … ${p.hi}` : ""}
                    </td>
                  </>
                )}
              </tr>
              {open === c.name && (
                <tr className="bg-hover/30">
                  <td colSpan={10}>
                    <TopValues e={explore} column={c.name} />
                  </td>
                </tr>
              )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
      {detail.foreign_keys.some((f) => f.columns.length > 1) && (
        <div className="space-y-0.5 border-t border-line px-3 py-2 text-[11.5px] text-muted">
          {detail.foreign_keys
            .filter((f) => f.columns.length > 1)
            .map((f) => (
              <div key={f.columns.join(",")}>
                <Link2 size={10} className="inline" /> ({f.columns.join(", ")}) → {f.ref_schema}.{f.ref_table} ({f.ref_columns.join(", ")})
              </div>
            ))}
        </div>
      )}
      </div>
    </div>
  );
}

/** The knowledge base's notes about the table and its columns; add one (it goes to the AI as the user's note). */
function NotesPart({ conn, notes, onSaved }: { conn: ConnectionView; notes: Notes; onSaved: () => void }) {
  const [body, setBody] = useState("");
  // The target the server reads back for this table (BigQuery projects, DuckDB results and files included).
  const base = notes.status === "ready" ? notes.notes.target : "";
  const [target, setTarget] = useState(base);
  const [saving, setSaving] = useState(false);
  useEffect(() => setTarget(base), [base]);
  const save = async () => {
    if (!body.trim() || !base) return;
    setSaving(true);
    try {
      await api.knSaveNote({ id: "", connection_id: conn.id, target: target.trim() || base, body: body.trim(), author: "user", status: "approved", created_at: 0, replaces: null });
      setBody("");
      onSaved();
      useStore.getState().toast("Note saved: the AI reads it with the table", "success");
    } catch (e) {
      useStore.getState().toast(toError(e).message, "error");
    } finally {
      setSaving(false);
    }
  };
  return (
    <div className="h-full space-y-3 overflow-auto p-3 text-[12px]">
      {notes.status === "loading" ? (
        <Busy />
      ) : notes.status === "error" ? (
        <p className="text-danger" role="alert">
          The notes couldn't be read: {notes.message}
        </p>
      ) : notes.notes.notes.length === 0 ? (
        <p className="text-muted">No notes about this table yet. A note tells the AI (and whoever reads the exported notes) what the table or a column means.</p>
      ) : (
        <ul className="space-y-1.5">
          {notes.notes.notes.map((n) => (
            <li key={n.id} className="rounded-md border border-line px-2 py-1.5">
              <div className="flex items-center gap-2 text-[11px] text-muted">
                <span className="font-mono">{n.target}</span>
                <span>
                  · {n.author === "user" ? "you" : n.author === "ai" ? "the AI" : n.author} · {relativeTime(n.created_at)}
                </span>
                {n.status === "proposed" && <span className="rounded bg-warning/15 px-1 text-warning">to review in Knowledge</span>}
              </div>
              <div className="mt-0.5 whitespace-pre-wrap break-words">{n.body}</div>
            </li>
          ))}
        </ul>
      )}
      <div className="space-y-1.5 rounded-md border border-line p-2">
        <div className="text-[11px] font-medium uppercase tracking-wide text-muted">Add a note</div>
        <input aria-label="Note about" className="field font-mono text-[11.5px]" value={target} onChange={(e) => setTarget(e.target.value)} title="This table's path as given, or that path followed by .column for a note on a column" />
        <textarea aria-label="Note" className="field min-h-[64px]" placeholder="e.g. total includes VAT; status 'void' rows are test orders" value={body} onChange={(e) => setBody(e.target.value)} />
        <div className="flex justify-end">
          <button className="btn-primary py-0.5 text-[12px]" disabled={!body.trim() || !base || saving} onClick={() => void save()}>
            {saving ? <Loader2 size={12} className="animate-spin" /> : <NotebookPen size={12} />} Save note
          </button>
        </div>
      </div>
    </div>
  );
}

// ------------------------------------------------------------------ profile and statistics (step 2)

/** The profile bar above the columns: Profile columns, its state and SQL. */
function ProfileBar({ e, detail, onProfile }: { e: Explore; detail: ObjectDetail; onProfile: (p: ReturnType<typeof readProfile> | null) => void }) {
  const kind = e.conn.config.kind;
  const { from } = localFrom(e);
  const built: Built = profileSql(kind, e.reference.schema, e.reference.name, detail.columns, from);
  const { ran, read, stop } = useExploreRead(e, "profile");
  const go = async () => {
    if (!("sql" in built)) return;
    const rows = await read(built.sql, "Profile the columns");
    onProfile(rows ? readProfile(rows, Math.min(detail.columns.length, PROFILE_MAX_COLUMNS)) : null);
  };
  if (!("sql" in built)) return <div className="border-b border-line px-2 py-1.5 text-[11.5px] text-muted">Profile: not available on {traitsOf(kind).name}: {built.unavailable}.</div>;
  return (
    <div className="shrink-0 border-b border-line">
      <div className="flex flex-wrap items-center gap-2 px-2 py-1.5 text-[12px]">
        {ran.status === "running" ? (
          <button className="btn-ghost border border-line py-0.5 text-[12px] text-danger" onClick={stop} title="Stop the profile query">
            <Square size={11} /> Stop
          </button>
        ) : (
          <button className="btn-ghost border border-line py-0.5 text-[12px]" onClick={() => void go()} title="One query reads every row: non-null share, distinct count, min and max of each column">
            <BarChart3 size={12} /> {ran.status === "done" ? "Profile again" : "Profile columns"}
          </button>
        )}
        {ran.status === "running" && (
          <span className="flex items-center gap-1 text-muted">
            <Loader2 size={12} className="animate-spin" /> Reading every row…
          </span>
        )}
        {ran.status === "idle" && (
          <span className="text-muted">
            Non-null share, distinct count{approxDistinct(kind) ? " (approximate)" : ""}, min and max of each column{detail.columns.length > PROFILE_MAX_COLUMNS ? ` (the first ${PROFILE_MAX_COLUMNS})` : ""}: one query that reads every row.
          </span>
        )}
        {ran.status === "error" && (
          <span className="flex min-w-0 items-start gap-1 text-danger" role="alert">
            <AlertTriangle size={12} className="mt-0.5 shrink-0" /> {ran.message}
          </span>
        )}
      </div>
      {ran.status !== "idle" && <RanSql e={e} sql={ran.sql} ms={ran.status === "done" ? ran.rows.duration_ms : undefined} />}
    </div>
  );
}

/** A column's most common values, under its row. */
function TopValues({ e, column }: { e: Explore; column: string }) {
  const kind = e.conn.config.kind;
  const { from } = localFrom(e);
  const built = topValuesSql(kind, e.reference.schema, e.reference.name, column, from);
  const { ran, read } = useExploreRead(e, `top:${column}`);
  const started = useRef(false);
  useEffect(() => {
    if (started.current || !("sql" in built)) return;
    started.current = true;
    void read(built.sql, "Read the most common values");
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once, when opened
  }, []);
  if (!("sql" in built)) return <p className="px-3 py-1 text-[11.5px] text-muted">{built.unavailable}</p>;
  if (ran.status === "idle") return <p className="px-3 py-1 text-[11.5px] text-muted">Not read.</p>;
  if (ran.status === "running") return <p className="flex items-center gap-1 px-3 py-1 text-[11.5px] text-muted"><Loader2 size={11} className="animate-spin" /> Counting values…</p>;
  if (ran.status === "error") return <p className="px-3 py-1 text-[11.5px] text-danger" role="alert">{ran.message}</p>;
  const rows = ran.rows.rows.map((r) => ({ value: r[0], n: num(r[1]) ?? 0 }));
  const max = Math.max(1, ...rows.map((r) => r.n));
  return (
    <div className="px-3 py-1.5">
      <div className="mb-1 text-[10.5px] uppercase tracking-wide text-muted">The {TOP_VALUES} most common values</div>
      <ul className="space-y-0.5 text-[11.5px]">
        {rows.map((r, i) => (
          <li key={i} className="flex items-center gap-2">
            <span className="w-48 shrink-0 truncate font-mono" title={r.value ?? "NULL"}>
              {r.value ?? <span className="italic text-muted">NULL</span>}
            </span>
            <span className="h-2 rounded-sm bg-accent/60" style={{ width: `${Math.max(2, (r.n / max) * 160)}px` }} />
            <span className="tabular-nums text-muted">{formatCount(r.n)}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** The table's statistics from the database's catalog, and an exact count on demand. */
function StatisticsPart({ explore: e }: { explore: Explore }) {
  const kind = e.conn.config.kind;
  const { from, local } = localFrom(e);
  const built = statisticsSql(kind, e.reference.schema, e.reference.name, e.reference.kind, local);
  const stats = useExploreRead(e, "stats");
  const count = useExploreRead(e, "count");
  const cbuilt = countSql(kind, e.reference.schema, e.reference.name, from);
  const started = useRef(false);
  const load = () => "sql" in built && void stats.read(built.sql, null);
  // The catalog's figures cost nothing: read when the part opens (once; Refresh reads them again).
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once, when opened
  }, []);
  const exact = count.ran.status === "done" ? num(count.ran.rows.rows[0]?.[0]) : null;
  const figures =
    stats.ran.status === "done"
      ? stats.ran.rows.columns.flatMap((c, i) => {
          // DESCRIBE DETAIL answers in camelCase; others as aliased (Oracle and Snowflake in upper case).
          const k = c.toLowerCase();
          const v = stats.ran.status === "done" ? (stats.ran.rows.rows[0]?.[i] ?? null) : null;
          if (v == null || v === "" || ["id", "name", "description", "properties", "minreaderversion", "minwriterversion", "tablefeatures", "statistics"].includes(k)) return [];
          const meta = STAT_LABELS[k];
          const n = num(v);
          const text = meta?.format === "bytes" && n != null ? formatBytes(n) : meta?.format === "count" && n != null ? (n < 0 ? "unknown (never analyzed)" : formatCount(n)) : v;
          return [{ key: k, label: meta?.label ?? c, text, raw: v }];
        })
      : [];
  return (
    <div className="flex h-full min-h-0 flex-col text-[12px]">
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line px-2 py-1.5">
        {"sql" in built && (
          <button className="btn-ghost border border-line py-0.5 text-[12px]" onClick={load} disabled={stats.ran.status === "running"} title="Read the catalog's figures again">
            <RefreshCw size={12} className={stats.ran.status === "running" ? "animate-spin" : ""} /> Refresh
          </button>
        )}
        {"sql" in cbuilt &&
          (count.ran.status === "running" ? (
            <button className="btn-ghost border border-line py-0.5 text-[12px] text-danger" onClick={count.stop}>
              <Square size={11} /> Stop counting
            </button>
          ) : (
            <button className="btn-ghost border border-line py-0.5 text-[12px]" onClick={() => void count.read(cbuilt.sql, "Count every row")} title="SELECT COUNT(*): reads every row">
              <Hash size={12} /> Count exactly
            </button>
          ))}
        {exact != null && <span>Exactly {formatCount(exact)} rows</span>}
        {count.ran.status === "error" && (
          <span className="text-danger" role="alert">
            {count.ran.message}
          </span>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-3">
        {!("sql" in built) ? (
          <p className="text-muted">Statistics: not available{traitsOf(kind).preview === "sql" ? ` on ${traitsOf(kind).name}` : ""}: {built.unavailable}.</p>
        ) : stats.ran.status === "running" || stats.ran.status === "idle" ? (
          <Busy />
        ) : stats.ran.status === "error" ? (
          <p className="text-danger" role="alert">
            The statistics couldn't be read: {stats.ran.message}
          </p>
        ) : figures.length === 0 ? (
          <p className="text-muted">The catalog has no figures for it{stats.ran.rows.rows.length === 0 ? " (it found no such table: Refresh the explorer?)" : ""}.</p>
        ) : (
          <dl className="grid max-w-2xl grid-cols-[max-content_1fr] gap-x-4 gap-y-1">
            {figures.map((f) => (
              <div key={f.key} className="contents">
                <dt className="text-muted">{f.label}</dt>
                <dd className="min-w-0 truncate font-mono text-[11.5px]" title={f.raw}>
                  {f.text}
                </dd>
              </div>
            ))}
          </dl>
        )}
        {"sql" in built && <p className="mt-3 text-[11px] text-muted">The database's own figures, kept by its catalog: estimates that are as fresh as its last analyze. Count exactly reads every row.</p>}
      </div>
      {stats.ran.status !== "idle" && <RanSql e={e} sql={stats.ran.sql} ms={stats.ran.status === "done" ? stats.ran.rows.duration_ms : undefined} />}
      {count.ran.status !== "idle" && count.ran.status !== "running" && <RanSql e={e} sql={count.ran.sql} ms={count.ran.status === "done" ? count.ran.rows.duration_ms : undefined} />}
    </div>
  );
}
