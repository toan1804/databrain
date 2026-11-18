// "Load into…": copy a result's fetched rows into a table of any connection,
// with the job load code (insert, delete then insert, replace, update or
// merge by key; a missing table is created). Also the last step of
// importing a CSV/Excel/Parquet file: query it, then load the result.

import { useEffect, useMemo, useRef, useState } from "react";
import { Loader2, Square, Upload } from "lucide-react";
import { api, toError } from "../lib/api";
import { useLoads } from "../loadsStore";
import type { LoadDryRun, LoadMode, LoadReport, ResultInfo } from "../lib/types";
import { formatCount, uid } from "../lib/util";
import { useStore } from "../store";
import { Warnings } from "./job/shared";
import { DryRunReport, LoadTargetCheck, isKeyedMode, loadModesFor, loadResultAsking, loadTargetBlocked, useLoadTarget } from "./job/LoadFields";
import { Modal } from "./ui";

/** The table generated test data was made for (its output's SQL is `-- Test data for <table> (…)`). */
export function testDataTable(sql: string): string | null {
  const m = /^-- Test data for (.+) \(\d+ rows, seed \d+\)$/.exec(sql.trim());
  return m ? m[1] : null;
}

/** A table name to suggest: the output's name, else the table the SQL reads. */
export function suggestTable(sql: string, outputName?: string | null): string {
  const generated = testDataTable(sql);
  if (generated) return generated;
  if (outputName) return outputName;
  const m = /\bfrom\s+((?:[\w$]+\.)?[\w$]+)/i.exec(sql);
  // Reading a file (read_csv('…'), 'file.csv'): its base name.
  const file = /['"]([^'"]*[\\/])?([^'"\\/]+?)\.(csv|tsv|txt|parquet|json|ndjson|jsonl|xlsx)(\.gz)?['"]/i.exec(sql);
  if (file) return file[2].toLowerCase().replace(/[^\w]+/g, "_");
  return m ? m[1].split(".").pop()! : "imported";
}

export function LoadIntoDialog({
  info,
  sql,
  outputName,
  connectionId,
  tabId,
  onClose,
}: {
  info: ResultInfo;
  sql: string;
  outputName?: string | null;
  /** Connection the rows came from: the default target for generated test data. */
  connectionId?: string;
  /** The result's tab: an "every row" load runs its query again on that tab's session. */
  tabId?: string;
  onClose: () => void;
}) {
  const toast = useStore((s) => s.toast);
  const connections = useStore((s) => s.connections);
  const writable = useMemo(() => connections.filter((c) => !c.config.read_only), [connections]);
  const generated = testDataTable(sql) !== null;
  // Generated test data goes back into the table it was made for, on its
  // own connection. Any other result has no default: its own connection
  // would offer its own table with Append (rows loaded twice), and the
  // first in the list is nobody's choice.
  const [target, setTarget] = useState((generated && writable.find((c) => c.id === connectionId)?.id) || "");
  const [table, setTable] = useState(() => suggestTable(sql, outputName));
  const [mode, setMode] = useState<LoadMode>("append");
  const [keys, setKeys] = useState<string[]>([]);
  const [create, setCreate] = useState(!generated);
  const [dry, setDry] = useState<{ busy: boolean; report?: LoadDryRun }>({ busy: false });
  // The load running from this dialog: its progress comes through the loads
  // store, which keeps following it if the dialog closes (the activity bar).
  const [runId, setRunId] = useState<string | null>(null);
  const run = useLoads((s) => (runId ? s.items.find((l) => l.id === runId && l.status === "running") : undefined)) ?? null;
  // Left (closed, "Continue in background") while it runs: the bar shows it from now on.
  const left = useRef(false);
  useEffect(
    () => () => {
      left.current = true;
      if (useLoads.getState().inDialog) useLoads.getState().setInDialog(null);
    },
    [],
  );
  const [report, setReport] = useState<LoadReport | null>(null);
  const targetKind = connections.find((c) => c.id === target)?.config.kind;
  const modes = useMemo(() => loadModesFor(targetKind), [targetKind]);
  // A target that can't do the chosen mode (ClickHouse and Merge): back to Insert.
  useEffect(() => {
    if (!modes.some((m) => m.mode === mode)) setMode("append");
  }, [modes, mode]);
  const keyed = isKeyedMode(mode);
  const hint = modes.find((m) => m.mode === mode)?.hint;
  // A result cut short (row limit or memory): run its query again and stream every row (default).
  const [everyRow, setEveryRow] = useState(info.truncated);
  const streaming = info.truncated && everyRow;
  const args = { target_connection_id: target, table, mode, key_columns: keyed ? keys : [], create_table: create, confirmed: false, every_row: streaming };
  const targetCheck = useLoadTarget(target, table);
  const blocked = loadTargetBlocked(targetCheck, mode, create);
  // Not while the table is being checked: what it says (it exists and is
  // dropped, or its columns don't fit) is what the user loads on.
  const checking = targetCheck.status === "checking";
  const ready = !!target && !!table.trim() && (!keyed || keys.length > 0) && !blocked && !checking;
  // A dry run is about the settings it was made with.
  const keyList = keys.join(",");
  useEffect(() => setDry({ busy: false }), [target, table, mode, keyList, create]);


  const check = async () => {
    setDry({ busy: true });
    try {
      setDry({ busy: false, report: await api.dryRunResultLoad(info.id, args) });
    } catch (e) {
      setDry({ busy: false });
      toast(toError(e).message, "error");
    }
  };
  const load = async () => {
    const id = uid();
    const loads = useLoads.getState();
    // Asked while it shows here; "Continue in background" or closing hands it to the bar.
    const shownHere = () => useLoads.getState().inDialog === id;
    try {
      // Progress and Stop once the load starts (after a production prompt).
      const r = await loadResultAsking(
        info.id,
        args,
        id,
        () => {
          loads.begin({ id, table, target: targetName || "the connection", total: streaming ? 0 : info.total_rows, tabId: streaming ? tabId : undefined });
          // Not after the dialog was left: a start again (after a production
          // prompt) would hide the load from the bar, its end with it.
          if (!left.current) loads.setInDialog(id);
          setRunId(id);
        },
        () => {
          loads.dismiss(id);
          setRunId(null);
        },
      );
      if (!r) return;
      const here = shownHere();
      loads.finish(id, r.rows);
      if (here) setReport(r);
    } catch (e) {
      const err = toError(e);
      const here = shownHere();
      if (err.kind === "cancelled") loads.stopped(id);
      else {
        loads.fail(id, err.message);
        if (here) toast(err.message, "error");
      }
    } finally {
      setRunId(null);
    }
  };
  // Leave the dialog, keep the load: the activity bar follows it.
  const toBackground = () => {
    left.current = true;
    useLoads.getState().setInDialog(null);
    onClose();
  };
  const targetName = connections.find((c) => c.id === target)?.name ?? "";

  return (
    <Modal
      title="Load rows into a table"
      onClose={() => (run ? toBackground() : onClose())}
      width={520}
      footer={
        report ? (
          <button className="btn-primary" onClick={onClose}>
            Close
          </button>
        ) : run ? (
          <>
            <button className="btn-ghost mr-auto" onClick={() => useLoads.getState().cancel(run.id)} disabled={run.stopping}>
              {run.stopping ? <Loader2 size={12} className="animate-spin" /> : <Square size={12} />} Stop
            </button>
            <button className="btn-ghost border border-line" onClick={toBackground} title="Close this dialog; the bar at the bottom shows the load until it ends">
              Continue in background
            </button>
          </>
        ) : (
          <>
            <button className="btn-ghost mr-auto border border-line" disabled={!ready || dry.busy} onClick={() => void check()} title="Run the load on up to 1,000 rows and roll it back (or check each statement), keeping nothing">
              {dry.busy ? <Loader2 size={13} className="animate-spin" /> : null} Dry run
            </button>
            <button className="btn-ghost" onClick={onClose}>
              Cancel
            </button>
            <button className="btn-primary" disabled={!ready} title={checking ? "Checking the table first…" : undefined} onClick={() => void load()}>
              <Upload size={13} /> {streaming ? "Load every row" : `Load ${formatCount(info.total_rows)} rows`}
            </button>
          </>
        )
      }
    >
      {report ? (
        <div className="space-y-1 text-[12.5px]" role="status">
          <p>
            {formatCount(report.rows)} rows written into <span className="font-mono">{table}</span> on {targetName}
            {report.created_table ? " (table created)" : ""}
            {report.deleted > 0 ? `, ${formatCount(report.deleted)} deleted` : ""}
            {report.transaction ? ", in one transaction" : ""}.
          </p>
          <Warnings warnings={report.warnings} />
          {report.notices.map((n, i) => (
            <p key={i} className="text-[11.5px] text-muted">
              {n}
            </p>
          ))}
        </div>
      ) : (
        <div className="space-y-3 text-[12.5px]">
          {info.truncated && (
            <div className="space-y-1.5 rounded-md bg-warning/10 px-2 py-1.5 text-[11.5px]" role="radiogroup" aria-label="Rows to load">
              <p className="text-warning">The result stopped at {formatCount(info.total_rows)} rows (the row limit, or the memory a result may keep).</p>
              <label className="flex cursor-pointer items-start gap-2">
                <input type="radio" name="every-row" className="mt-0.5" checked={everyRow} onChange={() => setEveryRow(true)} />
                <span>Every row: run the query again and write its rows as they arrive (they aren't all kept in memory)</span>
              </label>
              <label className="flex cursor-pointer items-start gap-2">
                <input type="radio" name="every-row" className="mt-0.5" checked={!everyRow} onChange={() => setEveryRow(false)} />
                <span>Only the {formatCount(info.total_rows)} rows of the result</span>
              </label>
            </div>
          )}
          <div className="grid grid-cols-2 gap-2">
            <label className="space-y-1">
              <span className="block text-[11.5px] font-medium text-muted">Into connection</span>
              <select className="field" value={target} onChange={(e) => setTarget(e.target.value)}>
                {writable.length === 0 && <option value="">No connection allows writes</option>}
                {writable.length > 0 && target === "" && <option value="">Choose a connection…</option>}
                {writable.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name}
                  </option>
                ))}
              </select>
            </label>
            <label className="space-y-1">
              <span className="block text-[11.5px] font-medium text-muted">Table</span>
              <input className="field font-mono" placeholder="schema.table" value={table} onChange={(e) => setTable(e.target.value)} />
            </label>
          </div>
          <label className="block space-y-1">
            <span className="block text-[11.5px] font-medium text-muted">Mode</span>
            <select className="field" value={mode} onChange={(e) => setMode(e.target.value as LoadMode)}>
              {modes.map((m) => (
                <option key={m.mode} value={m.mode}>
                  {m.text}
                </option>
              ))}
            </select>
            {hint && <span className="block text-[11.5px] text-muted">{hint}</span>}
          </label>
          {keyed && (
            <fieldset className="space-y-1">
              <legend className="text-[11.5px] font-medium text-muted">Key columns (identify a row)</legend>
              <div className="flex max-h-28 flex-wrap gap-1 overflow-auto">
                {info.columns.map((c) => (
                  <label key={c.name} className={`flex items-center gap-1 rounded border px-1.5 py-0.5 font-mono text-[11.5px] ${keys.includes(c.name) ? "border-accent" : "border-line"}`}>
                    <input
                      type="checkbox"
                      checked={keys.includes(c.name)}
                      onChange={(e) => setKeys((k) => (e.target.checked ? [...k, c.name] : k.filter((x) => x !== c.name)))}
                    />
                    {c.name}
                  </label>
                ))}
              </div>
            </fieldset>
          )}
          <LoadTargetCheck check={targetCheck} mode={mode} create={create} connName={connections.find((c) => c.id === target)?.name} onUse={setTable} />
          {(mode === "append" || mode === "truncate") && (
            <label className="flex items-center gap-2">
              <input type="checkbox" checked={create} onChange={(e) => setCreate(e.target.checked)} /> Create the table when it doesn't exist
            </label>
          )}
          {run && (
            <div className="space-y-1" role="status">
              <div className="flex items-center gap-2 text-[11.5px] text-muted">
                <Loader2 size={12} className="animate-spin" /> Writing {formatCount(run.done)}
                {run.total ? ` of ${formatCount(run.total)}` : ""} rows…
              </div>
              <div className="h-1.5 overflow-hidden rounded bg-hover">
                {run.total ? (
                  <div className="h-full bg-accent transition-all" style={{ width: `${Math.min(100, (run.done / run.total) * 100)}%` }} />
                ) : (
                  <div className="h-full w-1/3 animate-pulse bg-accent" />
                )}
              </div>
            </div>
          )}
          {dry.report && <DryRunReport report={dry.report} />}
        </div>
      )}
    </Modal>
  );
}
