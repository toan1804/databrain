// Settings of a load step (target, mode, keys, batch size, before/after SQL),
import { useEvent } from "../../lib/hooks";
// the target check and the dry run report (also used by "Load into…").
import { useEffect, useState } from "react";
import { AlertTriangle, FlaskConical, Info, CheckCircle2, CircleDashed, Loader2, Plus, XCircle } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { BatchLimits, Job, JobNode, LoadDryRun, LoadMode, LoadTarget } from "../../lib/types";
import { copyText } from "../CatalogMenus";
import { parseKeyColumns, batchValue } from "../../lib/jobGraph";
import { formatCount } from "../../lib/util";
import { traitsOf } from "../../lib/sourceTraits";
import { useStore } from "../../store";
import { CellEditor } from "../Notebook";
import { EDITOR_KEY, field, label } from "./shared";

/** Rows and KB per INSERT, within what the target connection allows. */
function BatchFields({ node, kind, onChange }: { node: JobNode; kind?: string; onChange: (patch: Partial<JobNode>) => void }) {
  const [limits, setLimits] = useState<BatchLimits | null>(null);
  useEffect(() => {
    if (!kind) return setLimits(null);
    let ok = true;
    api
      .loadBatchLimits(kind)
      .then((l) => ok && setLimits(l))
      .catch(() => ok && setLimits(null));
    return () => {
      ok = false;
    };
  }, [kind]);
  const [rows, setRows] = useState(node.batch_rows ? String(node.batch_rows) : "");
  const [kb, setKb] = useState(node.batch_kb ? String(node.batch_kb) : "");
  useEffect(() => {
    setRows(node.batch_rows ? String(node.batch_rows) : "");
    setKb(node.batch_kb ? String(node.batch_kb) : "");
  }, [node.id, node.batch_rows, node.batch_kb]);
  // A saved value past a new target's maximum is lowered at once.
  const clamp = useEvent((limits: BatchLimits) => {
    const patch: Partial<JobNode> = {};
    if (node.batch_rows && node.batch_rows > limits.max_rows) patch.batch_rows = limits.max_rows;
    if (node.batch_kb && node.batch_kb * 1024 > limits.max_bytes) patch.batch_kb = Math.floor(limits.max_bytes / 1024);
    if (Object.keys(patch).length) onChange(patch);
  });
  useEffect(() => {
    if (limits) clamp(limits);
  }, [limits, clamp]);
  const maxKb = limits ? Math.floor(limits.max_bytes / 1024) : 65536;
  const over = (text: string, max: number) => !!text.trim() && Number(text) > max;
  return (
    <div>
      <div className="grid grid-cols-2 gap-2">
        <div>
          <label className={label} htmlFor={`batch-rows-${node.id}`}>
            Rows per INSERT
          </label>
          <input
            id={`batch-rows-${node.id}`}
            type="number"
            min={1}
            max={limits?.max_rows}
            step={1}
            inputMode="numeric"
            className={`${field} ${limits && over(rows, limits.max_rows) ? "border-warning" : ""}`}
            placeholder={limits ? `${formatCount(limits.default_rows)} (default)` : "default"}
            value={rows}
            onChange={(e) => setRows(e.target.value)}
            onBlur={() => {
              const v = batchValue(rows, limits?.max_rows ?? Number.MAX_SAFE_INTEGER);
              setRows(v ? String(v) : "");
              onChange({ batch_rows: v });
            }}
            aria-describedby={`batch-help-${node.id}`}
          />
        </div>
        <div>
          <label className={label} htmlFor={`batch-kb-${node.id}`}>
            Max KB per INSERT
          </label>
          <input
            id={`batch-kb-${node.id}`}
            type="number"
            min={1}
            max={maxKb}
            step={1}
            inputMode="numeric"
            className={`${field} ${limits && over(kb, maxKb) ? "border-warning" : ""}`}
            placeholder={limits ? `${formatCount(Math.floor(limits.default_bytes / 1024))} (default)` : "default"}
            value={kb}
            onChange={(e) => setKb(e.target.value)}
            onBlur={() => {
              const v = batchValue(kb, maxKb);
              setKb(v ? String(v) : "");
              onChange({ batch_kb: v });
            }}
            aria-describedby={`batch-help-${node.id}`}
          />
        </div>
      </div>
      <div id={`batch-help-${node.id}`} className="mt-0.5 space-y-0.5 text-[11px] text-muted">
        <div>A statement ends at whichever comes first: the rows or the size of its SQL text. Lower them when the database rejects large statements.</div>
        {limits ? (
          <>
            <div>
              At most {formatCount(limits.max_rows)} rows: {limits.rows_note}.
            </div>
            <div>
              At most {formatCount(maxKb)} KB: {limits.bytes_note}.
            </div>
          </>
        ) : (
          <div>Choose the connection to see its limits.</div>
        )}
      </div>
    </div>
  );
}

export const LOAD_MODES: { mode: LoadMode; text: string; hint: string }[] = [
  { mode: "append", text: "Insert", hint: "Insert the rows. A missing table can be created from the rows' columns." },
  { mode: "truncate", text: "Delete rows, then insert", hint: "Delete every row of the table, then insert. A missing table can be created." },
  { mode: "replace", text: "Replace table", hint: "Drop the table if it exists and create it again from the rows' columns, then insert." },
  { mode: "update", text: "Update by key", hint: "The table must exist. Rows whose key columns match are updated; the others are ignored." },
  { mode: "merge", text: "Merge (upsert) by key", hint: "The table must exist. Rows whose key columns match are updated, the others inserted." },
  {
    mode: "sync",
    text: "Sync (mirror) by key",
    hint: "The table must exist. Like Merge, then the table's rows whose key isn't among the rows are deleted: it ends up holding exactly these rows. A step without rows is refused.",
  },
];

/** Modes that match rows by key columns. */
export function isKeyedMode(mode: LoadMode | undefined | null): boolean {
  return mode === "update" || mode === "merge" || mode === "sync";
}

/** Modes a target of `kind` can do; `current` stays listed (marked) so a
 * step saved with it shows what it has. */
export function loadModesFor(kind: string | undefined, current?: LoadMode): { mode: LoadMode; text: string; hint: string }[] {
  const keyed = traitsOf(kind).load?.keyed !== false;
  return LOAD_MODES.flatMap((m) => {
    const ok = keyed || !isKeyedMode(m.mode);
    if (ok) return [m];
    if (m.mode !== current) return [];
    return [{ ...m, text: `${m.text} (not on ${traitsOf(kind).name})`, hint: `${traitsOf(kind).name} can't update rows by key: choose another mode.` }];
  });
}

/** Target, mode, key columns, before/after SQL and dry run of a load step. */
export function LoadFields({ job, node, onChange }: { job: Job; node: JobNode; onChange: (patch: Partial<JobNode>) => void }) {
  const connections = useStore((s) => s.connections);
  const target = connections.find((c) => c.id === node.target_connection_id);
  const modes = loadModesFor(target?.config.kind, node.load_mode);
  const mode = modes.find((m) => m.mode === node.load_mode) ?? LOAD_MODES[0];
  const keyed = isKeyedMode(node.load_mode);
  const [keys, setKeys] = useState((node.key_columns ?? []).join(", "));
  // Reset the text for another step (not while this one is being edited).
  const savedKeys = useEvent(() => (node.key_columns ?? []).join(", "));
  useEffect(() => setKeys(savedKeys()), [node.id, savedKeys]);
  const [dry, setDry] = useState<{ busy: boolean; report?: LoadDryRun; error?: string }>({ busy: false });
  // A report is about the settings it was made with.
  useEffect(() => setDry({ busy: false }), [node.id, node.target_connection_id, node.target_table, node.load_mode]);
  const targetCheck = useLoadTarget(node.target_connection_id, node.target_table);
  const [showBefore, setShowBefore] = useState(!!node.load_before_sql?.trim());
  const [showAfter, setShowAfter] = useState(!!node.load_after_sql?.trim());
  const runDry = async () => {
    setDry({ busy: true });
    try {
      setDry({ busy: false, report: await api.dryRunJobStep(job, node.id) });
    } catch (e) {
      setDry({ busy: false, error: toError(e).message });
    }
  };
  const sqlBox = (which: "before" | "after", value: string | null | undefined) => (
    <div>
      <label className={label}>
        {which === "before" ? "Before SQL" : "After SQL"} (on {target?.name ?? "the target connection"}, {which === "before" ? "before the rows are written" : "after they are written"})
      </label>
      <CellEditor
        editorKey={`${EDITOR_KEY(job.id, node.id)}:${which}`}
        source={value ?? ""}
        connectionId={node.target_connection_id ?? null}
        height={110}
        onResize={() => {}}
        onChange={(sql) => onChange(which === "before" ? { load_before_sql: sql } : { load_after_sql: sql })}
        onRun={() => {}}
        onRunAdvance={() => {}}
        onFocus={() => {}}
      />
    </div>
  );
  return (
    <div className="space-y-2 rounded-md border border-line p-2">
      <div className="grid grid-cols-2 gap-2">
        <div>
          <label className={label} htmlFor={`target-${node.id}`}>
            Into connection
          </label>
          <select id={`target-${node.id}`} className={field} value={node.target_connection_id ?? ""} onChange={(e) => onChange({ target_connection_id: e.target.value || null })}>
            <option value="">Choose…</option>
            {connections.map((c) => (
              <option key={c.id} value={c.id} disabled={c.config.read_only}>
                {c.name}
                {c.config.read_only ? " (read-only)" : ""}
              </option>
            ))}
          </select>
        </div>
        <div>
          <label className={label} htmlFor={`table-${node.id}`}>
            Table
          </label>
          <input id={`table-${node.id}`} className={`${field} font-mono`} placeholder="schema.table" value={node.target_table ?? ""} onChange={(e) => onChange({ target_table: e.target.value })} />
        </div>
      </div>
      <LoadTargetCheck
        check={targetCheck}
        mode={node.load_mode ?? "append"}
        create={node.create_table ?? true}
        beforeSql={!!node.load_before_sql?.trim()}
        connName={target?.name}
        onUse={(p) => onChange({ target_table: p })}
      />
      <div>
        <label className={label} htmlFor={`mode-${node.id}`}>
          Mode
        </label>
        <select id={`mode-${node.id}`} className={field} value={mode.mode} onChange={(e) => onChange({ load_mode: e.target.value as LoadMode })}>
          {modes.map((m) => (
            <option key={m.mode} value={m.mode}>
              {m.text}
            </option>
          ))}
        </select>
        <div className="mt-0.5 text-[11px] text-muted">{mode.hint}</div>
      </div>
      {(node.load_mode === "append" || node.load_mode === "truncate") && (
        <label className="flex items-center gap-1.5 text-[12px]">
          <input type="checkbox" checked={node.create_table ?? true} onChange={(e) => onChange({ create_table: e.target.checked })} />
          Create the table if it's missing (from the rows' columns; you get a notice)
        </label>
      )}
      {keyed && (
        <div>
          <label className={label} htmlFor={`keys-${node.id}`}>
            Key columns
          </label>
          <input
            id={`keys-${node.id}`}
            className={`${field} font-mono ${parseKeyColumns(keys).length ? "" : "border-warning"}`}
            placeholder="id, region"
            value={keys}
            onChange={(e) => setKeys(e.target.value)}
            onBlur={() => onChange({ key_columns: parseKeyColumns(keys) })}
          />
          <div className="mt-0.5 text-[11px] text-muted">
            {parseKeyColumns(keys).length ? "Comma-separated columns that identify a row (in the rows and the table)." : "Required: the columns that identify a row, comma-separated."}
          </div>
        </div>
      )}
      <BatchFields node={node} kind={target?.config.kind} onChange={onChange} />
      <div className="flex flex-wrap gap-1.5">
        {!showBefore && (
          <button className="btn-ghost border border-line py-0.5 text-[11.5px]" onClick={() => setShowBefore(true)}>
            <Plus size={12} /> Before SQL
          </button>
        )}
        {!showAfter && (
          <button className="btn-ghost border border-line py-0.5 text-[11.5px]" onClick={() => setShowAfter(true)}>
            <Plus size={12} /> After SQL
          </button>
        )}
      </div>
      {showBefore && sqlBox("before", node.load_before_sql)}
      {showAfter && sqlBox("after", node.load_after_sql)}
      {target?.env === "prod" && (
        <div className="flex items-center gap-1 text-[11.5px] text-warning">
          <AlertTriangle size={12} /> {target.name} is a production connection: runs write to it without asking.
        </div>
      )}
      <div className="flex items-center gap-2">
        <button
          className="btn-ghost border border-line py-1 text-[12px]"
          disabled={dry.busy || !node.target_connection_id}
          title="Check every statement of this step (before SQL, the load, after SQL) without keeping any change"
          onClick={() => void runDry()}
        >
          {dry.busy ? <Loader2 size={12} className="animate-spin" /> : <FlaskConical size={12} />} Dry run
        </button>
        <span className="text-[11px] text-muted">Checks the SQL with up to 1,000 of the rows; nothing is kept.</span>
      </div>
      {dry.error && <div className="text-[11.5px] text-danger">{dry.error}</div>}
      {dry.report && <DryRunReport report={dry.report} />}
    </div>
  );
}

export type TargetCheck = { status: "idle" } | { status: "checking" } | { status: "ready"; target: LoadTarget } | { status: "error"; message: string };

/** Check the load target path while it is typed (debounced; live from the database). */
export function useLoadTarget(connectionId: string | null | undefined, table: string | null | undefined): TargetCheck {
  const [state, setState] = useState<TargetCheck>({ status: "idle" });
  useEffect(() => {
    const t = table?.trim() ?? "";
    if (!connectionId || !t) return setState({ status: "idle" });
    let live = true;
    setState({ status: "checking" });
    const timer = setTimeout(() => {
      api
        .checkLoadTarget(connectionId, t)
        .then((target) => live && setState({ status: "ready", target }))
        .catch((e) => live && setState({ status: "error", message: toError(e).message }));
    }, 400);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [connectionId, table]);
  return state;
}

/**
 * Rows can't be loaded as set up: the catalog or schema is missing, the
 * target is a view, or the table is missing in a mode that doesn't create it
 * (not when before SQL runs first: it may create them).
 */
export function loadTargetBlocked(c: TargetCheck, mode: LoadMode, create: boolean, beforeSql = false): boolean {
  if (c.status !== "ready" || beforeSql) return false;
  const t = c.target;
  if (t.problems.length) return true;
  const creates = mode === "replace" || ((mode === "append" || mode === "truncate") && create);
  return t.table_exists === false && !creates;
}

/** What the target check found, under the table field: what exists, and what to create first. */
export function LoadTargetCheck({
  check,
  mode,
  create,
  beforeSql = false,
  connName,
  onUse,
}: {
  check: TargetCheck;
  mode: LoadMode;
  create: boolean;
  beforeSql?: boolean;
  connName?: string;
  onUse: (path: string) => void;
}) {
  if (check.status === "idle") return null;
  if (check.status === "checking")
    return (
      <div className="flex items-center gap-1.5 text-[11.5px] text-muted" role="status">
        <Loader2 size={12} className="animate-spin" /> Checking the table path{connName ? ` on ${connName}` : ""}…
      </div>
    );
  if (check.status === "error")
    return (
      <div className="flex items-start gap-1.5 text-[11.5px] text-muted" role="status">
        <CircleDashed size={12} className="mt-0.5 shrink-0" /> <span className="min-w-0 break-words">Couldn't check the path: {check.message}</span>
      </div>
    );
  const t = check.target;
  const creates = mode === "replace" || ((mode === "append" || mode === "truncate") && create);
  const blocked = loadTargetBlocked(check, mode, create, beforeSql);
  const suggestion = t.suggestion && (
    <button className="btn-ghost h-6 border border-line px-1.5 py-0 font-mono text-[11px]" onClick={() => onUse(t.suggestion!)} title="Use the name as the database spells it">
      Use {t.suggestion}
    </button>
  );
  if (t.problems.length) {
    return (
      <div className={`space-y-1 rounded-md border px-2 py-1.5 text-[11.5px] ${beforeSql ? "border-warning/50 bg-warning/5" : "border-danger/50 bg-danger/5"}`} role="alert">
        {t.problems.map((p, i) => (
          <div key={i} className={`flex items-start gap-1.5 ${beforeSql ? "text-warning" : "text-danger"}`}>
            <XCircle size={12} className="mt-0.5 shrink-0" /> <span className="min-w-0 break-words">{p}</span>
          </div>
        ))}
        {t.create_sql && (
          <div className="flex items-center gap-1">
            <code className="min-w-0 flex-1 truncate rounded bg-panel-2 px-1.5 py-0.5 font-mono text-[11px]" title={t.create_sql}>
              {t.create_sql}
            </code>
            <button className="btn-ghost h-6 shrink-0 border border-line px-1.5 py-0 text-[11px]" onClick={() => void copyText(t.create_sql!, "SQL")}>
              Copy
            </button>
          </div>
        )}
        {suggestion}
        {beforeSql && <div className="text-muted">The before SQL runs first: it may create it.</div>}
      </div>
    );
  }
  const exists = t.table_exists === true;
  const missing = t.table_exists === false;
  const what = exists
    ? mode === "replace"
      ? "exists: it is dropped and created again from the rows' columns"
      : mode === "truncate"
        ? "exists: its rows are deleted, then the rows inserted"
        : isKeyedMode(mode)
          ? mode === "sync"
            ? "exists: rows are matched by the key columns, and its rows missing from them are deleted"
            : "exists: rows are matched by the key columns"
          : "exists: the rows are added to it"
    : missing
      ? creates
        ? "doesn't exist yet: it is created from the rows' columns"
        : `doesn't exist: create it first${mode === "append" || mode === "truncate" ? ', or turn on "Create the table"' : ` (${mode === "update" ? "Update" : mode === "sync" ? "Sync" : "Merge"} doesn't create tables)`}`
      : "";
  return (
    <div className="space-y-0.5 text-[11.5px]" role="status">
      {what && (
        <div className={`flex items-start gap-1.5 ${blocked ? "text-danger" : exists ? "text-success" : "text-accent"}`}>
          {blocked ? <XCircle size={12} className="mt-0.5 shrink-0" /> : exists ? <CheckCircle2 size={12} className="mt-0.5 shrink-0" /> : <Info size={12} className="mt-0.5 shrink-0" />}
          <span className="min-w-0 break-words">
            <span className="font-mono">{t.resolved}</span> {what}
            {missing && beforeSql && !creates ? " (or the before SQL creates it)" : ""}
          </span>
        </div>
      )}
      {t.notes.map((n, i) => (
        <div key={i} className="flex items-start gap-1.5 text-muted">
          <Info size={12} className="mt-0.5 shrink-0" /> <span className="min-w-0 break-words">{n}</span>
        </div>
      ))}
      {suggestion}
    </div>
  );
}

export function DryRunReport({ report }: { report: LoadDryRun }) {
  const how =
    report.method === "transaction"
      ? `Ran on ${report.sample_rows != null ? `${formatCount(report.sample_rows)} sample rows` : "no rows"} in a transaction, then rolled back`
      : report.method === "explain"
        ? "Checked statement by statement without running them (this database can't roll the whole step back)"
        : "The step could not be planned";
  return (
    <div className={`rounded-md border px-2 py-1.5 text-[11.5px] ${report.ok ? "border-success/50" : "border-danger/50 bg-danger/5"}`} role="status">
      <div className="flex items-center gap-1.5 font-medium">
        {report.ok ? <CheckCircle2 size={12} className="text-success" /> : <XCircle size={12} className="text-danger" />}
        {report.ok ? "Dry run passed" : "Dry run found problems"}
      </div>
      <div className="text-muted">
        {how}.
        {report.batch && ` INSERTs of up to ${formatCount(report.batch.rows)} rows / ${formatCount(Math.floor(report.batch.bytes / 1024))} KB.`}
      </div>
      {report.rows_error && <div className="mt-0.5 text-danger">Rows: {report.rows_error}</div>}
      {report.notices.map((n, i) => (
        <div key={i} className="mt-0.5 flex items-start gap-1 text-accent">
          <Info size={12} className="mt-0.5 shrink-0" /> <span className="min-w-0 break-words">{n}</span>
        </div>
      ))}
      <ul className="mt-1 space-y-0.5">
        {report.checks.map((c, i) => (
          <li key={i} className="flex items-start gap-1.5">
            {c.status === "ok" ? (
              <CheckCircle2 size={12} className="mt-0.5 shrink-0 text-success" />
            ) : c.status === "error" ? (
              <XCircle size={12} className="mt-0.5 shrink-0 text-danger" />
            ) : c.status === "warning" ? (
              <AlertTriangle size={12} className="mt-0.5 shrink-0 text-warning" />
            ) : (
              <CircleDashed size={12} className="mt-0.5 shrink-0 text-muted" />
            )}
            <div className="min-w-0 flex-1">
              <span className="font-medium">{c.label}</span>
              {c.message && <span className={c.status === "error" ? " text-danger" : " text-muted"}> · {c.message}</span>}
              {c.sql && (
                <div className="truncate font-mono text-[10.5px] text-muted" title={c.sql}>
                  {c.sql}
                </div>
              )}
            </div>
          </li>
        ))}
      </ul>
    </div>
  );
}
