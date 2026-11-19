// Table explorer: one tab per table or view that gathers what DataBrain
// knows and can read about it: sample rows, columns, DDL and notes (see
// docs/TODO.md "Table explorer" for the later parts: column profile,
// statistics, relations, history, usage).
import { useEffect, useMemo, useRef, useState } from "react";
import { FileCode2, Key, Link2, Loader2, NotebookPen, Play, RefreshCw, Shuffle, Table2 } from "lucide-react";
import { api, toError } from "../lib/api";
import { tablePath } from "../lib/catalog";
import { traitsOf } from "../lib/sourceTraits";
import type { ConnectionView, KnNote, ObjectDetail } from "../lib/types";
import { formatCount, relativeTime, sampleSql, shortCount } from "../lib/util";
import { useStore, type DdlRef } from "../store";
import { DdlTab } from "./DdlTab";
import { ResultsPanel } from "./ResultsPanel";
import { ConnDot, EnvBadge } from "./ui";
import { selectTop } from "./CatalogSearch";


type Part = "data" | "columns" | "ddl" | "notes";
type Detail = { status: "loading" } | { status: "ready"; detail: ObjectDetail } | { status: "error"; message: string };

export function TableTab({ tabId, reference, visible }: { tabId: string; reference: DdlRef; visible: boolean }) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const conn = useStore((s) => s.connections.find((c) => c.id === tab?.connection_id));
  const [part, setPart] = useState<Part>("data");
  const [detail, setDetail] = useState<Detail>({ status: "loading" });
  const [notes, setNotes] = useState<KnNote[] | null>(null);
  const [seq, setSeq] = useState(0);
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
    api
      .knTableNotes(connId, reference.schema, reference.name)
      .then((n) => live && setNotes(n))
      .catch(() => live && setNotes([]));
    return () => {
      live = false;
    };
  }, [connId, reference.schema, reference.name, seq]);

  if (!conn || !kind) return <div className="p-4 text-[12.5px] text-muted" style={{ display: visible ? "block" : "none" }}>The connection of this tab no longer exists.</div>;
  const d = detail.status === "ready" ? detail.detail : null;
  const object = d?.object;
  const columns = d?.columns ?? [];
  const loadable = reference.kind === "table" && !!traitsOf(kind).load;
  const parts: { key: Part; label: string; count?: number }[] = [
    { key: "data", label: "Data" },
    { key: "columns", label: "Columns", count: d ? columns.length : undefined },
    { key: "ddl", label: "DDL" },
    { key: "notes", label: "Notes", count: notes?.length || undefined },
  ];

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
          <button className="icon-btn ml-auto h-6 w-6 shrink-0" onClick={() => setSeq((n) => n + 1)} title="Read the table again from the database" aria-label="Refresh">
            <RefreshCw size={12} className={detail.status === "loading" ? "animate-spin" : ""} />
          </button>
        </div>
        {object?.comment && <div className="truncate text-[11.5px] text-muted" title={object.comment}>{object.comment}</div>}
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
            onClick={() => setPart(p.key)}
            className={`-mb-px border-b-2 px-2 py-1.5 text-[12px] ${part === p.key ? "border-accent text-fg" : "border-transparent text-muted hover:text-fg"}`}
          >
            {p.label}
            {p.count != null && <span className="ml-1 text-[10.5px] text-muted">{p.count}</span>}
          </button>
        ))}
      </nav>
      <div className="min-h-0 flex-1">
        {part === "data" && <DataPart tabId={tabId} conn={conn} reference={reference} />}
        {part === "columns" &&
          (detail.status === "loading" ? (
            <Busy />
          ) : detail.status === "error" ? (
            <p className="p-3 text-[12px] text-danger">{detail.message}</p>
          ) : (
            <ColumnsPart detail={detail.detail} notes={notes ?? []} />
          ))}
        {/* The DDL view as Show DDL opens it, in this tab. */}
        {part === "ddl" && <DdlTab tabId={tabId} reference={reference} visible />}
        {part === "notes" && <NotesPart conn={conn} path={path} notes={notes} onSaved={() => setSeq((n) => n + 1)} />}
      </div>
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
  const sql = useMemo(() => sampleSql(kind, reference.schema, reference.name, n, filter), [kind, reference.schema, reference.name, n, filter]);
  const run = useStore((s) => s.runs[tabId]);
  const [ran, setRan] = useState<string | null>(null);
  const go = (text = sql) => {
    setRan(text);
    void useStore.getState().runSql(tabId, conn.id, text, 0, undefined, undefined, undefined, n);
  };
  // The first rows as soon as the tab opens (once: React runs effects twice
  // in development, and the run isn't in the store yet; Run reads them again).
  const started = useRef(false);
  useEffect(() => {
    if (started.current || run) return;
    started.current = true;
    go();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once, when the tab opens; Run reads the rows again
  }, []);
  return (
    <div className="flex h-full min-h-0 flex-col">
      <form
        className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line px-2 py-1.5 text-[12px]"
        onSubmit={(e) => {
          e.preventDefault();
          go();
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
        {filterable && (
          <input
            aria-label="Where condition"
            className="min-w-[200px] flex-1 rounded border border-line bg-bg px-2 py-0.5 font-mono text-[11.5px]"
            placeholder="where …  e.g. status = 'paid'"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        )}
        <button type="submit" className="btn-primary py-0.5 text-[12px]" disabled={run?.running}>
          {run?.running ? <Loader2 size={12} className="animate-spin" /> : <Play size={12} />} Run
        </button>
        <button
          type="button"
          className="btn-ghost py-0.5 text-[12px]"
          onClick={() => useStore.getState().newTab({ title: reference.name, sql: ran ?? sql, connection_id: conn.id })}
          title="A query tab with this SQL"
        >
          <FileCode2 size={12} /> Open in editor
        </button>
      </form>
      <div className="min-h-0 flex-1">
        <ResultsPanel tabId={tabId} connectionId={conn.id} title={reference.name} />
      </div>
    </div>
  );
}

/** The columns as the database describes them, with keys, comments and notes. */
function ColumnsPart({ detail, notes }: { detail: ObjectDetail; notes: KnNote[] }) {
  const fkOf = (col: string) => detail.foreign_keys.find((f) => f.columns.length === 1 && f.columns[0].toLowerCase() === col.toLowerCase());
  const noteOf = (col: string) =>
    notes.find((n) => {
      const last = (n.target ?? "").split(".").pop()?.replace(/["`[\]]/g, "").toLowerCase();
      return last === col.toLowerCase();
    });
  if (!detail.columns.length) return <p className="p-3 text-[12px] text-muted">The database lists no columns for it.</p>;
  return (
    <div className="h-full overflow-auto">
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
          </tr>
        </thead>
        <tbody>
          {detail.columns.map((c, i) => {
            const fk = fkOf(c.name);
            const note = noteOf(c.name);
            return (
              <tr key={c.name} className="border-t border-line/60 hover:bg-hover/40">
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
                    <div className="flex items-start gap-1 truncate text-accent" title={note.body}>
                      <NotebookPen size={10} className="mt-0.5 shrink-0" /> {note.body}
                    </div>
                  )}
                </td>
              </tr>
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
  );
}

/** The knowledge base's notes about the table and its columns; add one (it goes to the AI as the user's note). */
function NotesPart({ conn, path, notes, onSaved }: { conn: ConnectionView; path: string; notes: KnNote[] | null; onSaved: () => void }) {
  const [body, setBody] = useState("");
  const [target, setTarget] = useState(path);
  const [saving, setSaving] = useState(false);
  useEffect(() => setTarget(path), [path]);
  const save = async () => {
    if (!body.trim()) return;
    setSaving(true);
    try {
      await api.knSaveNote({ id: "", connection_id: conn.id, target: target.trim() || path, body: body.trim(), author: "user", status: "approved", created_at: 0, replaces: null });
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
      {notes === null ? (
        <Busy />
      ) : notes.length === 0 ? (
        <p className="text-muted">No notes about this table yet. A note tells the AI (and whoever reads the exported notes) what the table or a column means.</p>
      ) : (
        <ul className="space-y-1.5">
          {notes.map((n) => (
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
        <input aria-label="Note about" className="field font-mono text-[11.5px]" value={target} onChange={(e) => setTarget(e.target.value)} title="The table, or table.column for a column" />
        <textarea aria-label="Note" className="field min-h-[64px]" placeholder="e.g. total includes VAT; status 'void' rows are test orders" value={body} onChange={(e) => setBody(e.target.value)} />
        <div className="flex justify-end">
          <button className="btn-primary py-0.5 text-[12px]" disabled={!body.trim() || saving} onClick={() => void save()}>
            {saving ? <Loader2 size={12} className="animate-spin" /> : <NotebookPen size={12} />} Save note
          </button>
        </div>
      </div>
    </div>
  );
}
