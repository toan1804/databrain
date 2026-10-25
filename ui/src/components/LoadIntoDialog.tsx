// "Load into…": copy a result's fetched rows into a table of any connection,
// with the job load code (insert, delete then insert, replace, update or
// merge by key; a missing table is created). Also the last step of
// importing a CSV/Excel/Parquet file: query it, then load the result.

import { useEffect, useMemo, useRef, useState } from "react";
import { Loader2, Square, Upload } from "lucide-react";
import { api, onLoadProgress, toError } from "../lib/api";
import type { LoadDryRun, LoadMode, LoadReport, ResultInfo } from "../lib/types";
import { formatCount, uid } from "../lib/util";
import { useStore } from "../store";
import { DryRunReport, LOAD_MODES, LoadTargetCheck, loadTargetBlocked, useLoadTarget } from "./JobTab";
import { Modal } from "./ui";

/** A table name to suggest: the output's name, else the table the SQL reads. */
export function suggestTable(sql: string, outputName?: string | null): string {
  if (outputName) return outputName;
  const m = /\bfrom\s+((?:[\w$]+\.)?[\w$]+)/i.exec(sql);
  // Reading a file (read_csv('…'), 'file.csv'): its base name.
  const file = /['"]([^'"]*[\\/])?([^'"\\/]+?)\.(csv|tsv|txt|parquet|json|ndjson|jsonl|xlsx)(\.gz)?['"]/i.exec(sql);
  if (file) return file[2].toLowerCase().replace(/[^\w]+/g, "_");
  return m ? m[1].split(".").pop()! : "imported";
}

export function LoadIntoDialog({ info, sql, outputName, onClose }: { info: ResultInfo; sql: string; outputName?: string | null; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const connections = useStore((s) => s.connections);
  const writable = useMemo(() => connections.filter((c) => !c.config.read_only), [connections]);
  const [target, setTarget] = useState(writable[0]?.id ?? "");
  const [table, setTable] = useState(() => suggestTable(sql, outputName));
  const [mode, setMode] = useState<LoadMode>("append");
  const [keys, setKeys] = useState<string[]>([]);
  const [create, setCreate] = useState(true);
  const [dry, setDry] = useState<{ busy: boolean; report?: LoadDryRun }>({ busy: false });
  const [run, setRun] = useState<{ id: string; done: number; total: number } | null>(null);
  const [report, setReport] = useState<LoadReport | null>(null);
  const keyed = mode === "update" || mode === "merge";
  const hint = LOAD_MODES.find((m) => m.mode === mode)?.hint;
  const args = { target_connection_id: target, table, mode, key_columns: keyed ? keys : [], create_table: create };
  const targetCheck = useLoadTarget(target, table);
  const blocked = loadTargetBlocked(targetCheck, mode, create);
  const ready = !!target && !!table.trim() && (!keyed || keys.length > 0) && !blocked;
  // A dry run is about the settings it was made with.
  useEffect(() => setDry({ busy: false }), [target, table, mode, keys.join(","), create]); // eslint-disable-line react-hooks/exhaustive-deps

  const runId = useRef<string | null>(null);
  useEffect(() => {
    const un = onLoadProgress((e) => e.load_id === runId.current && setRun((r) => (r ? { ...r, done: e.done, total: e.total } : r)));
    return () => void un.then((f) => f());
  }, []);

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
    runId.current = id;
    setRun({ id, done: 0, total: info.total_rows });
    try {
      const r = await api.loadResult(info.id, args, id);
      setReport(r);
      toast(`Loaded ${formatCount(r.rows)} rows into ${table}`, "success");
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setRun(null);
      runId.current = null;
    }
  };
  const targetName = connections.find((c) => c.id === target)?.name ?? "";

  return (
    <Modal
      title="Load rows into a table"
      onClose={() => !run && onClose()}
      width={520}
      footer={
        report ? (
          <button className="btn-primary" onClick={onClose}>
            Close
          </button>
        ) : run ? (
          <button className="btn-ghost mr-auto" onClick={() => void api.cancelLoad(run.id)}>
            <Square size={12} /> Stop
          </button>
        ) : (
          <>
            <button className="btn-ghost mr-auto border border-line" disabled={!ready || dry.busy} onClick={() => void check()} title="Run the load on up to 1,000 rows and roll it back (or check each statement), keeping nothing">
              {dry.busy ? <Loader2 size={13} className="animate-spin" /> : null} Dry run
            </button>
            <button className="btn-ghost" onClick={onClose}>
              Cancel
            </button>
            <button className="btn-primary" disabled={!ready} onClick={() => void load()}>
              <Upload size={13} /> Load {formatCount(info.total_rows)} rows
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
            {report.transaction ? ", in one transaction" : ""}.
          </p>
          {report.notices.map((n, i) => (
            <p key={i} className="text-[11.5px] text-muted">
              {n}
            </p>
          ))}
        </div>
      ) : (
        <div className="space-y-3 text-[12.5px]">
          {info.truncated && (
            <p className="rounded-md bg-warning/10 px-2 py-1.5 text-[11.5px] text-warning">
              The result was limited to {formatCount(info.total_rows)} rows: only those are loaded. Raise the row limit and run again, or use a job to load
              everything.
            </p>
          )}
          <div className="grid grid-cols-2 gap-2">
            <label className="space-y-1">
              <span className="block text-[11.5px] font-medium text-muted">Into connection</span>
              <select className="field" value={target} onChange={(e) => setTarget(e.target.value)}>
                {writable.length === 0 && <option value="">No connection allows writes</option>}
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
              {LOAD_MODES.map((m) => (
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
                <Loader2 size={12} className="animate-spin" /> Writing {formatCount(run.done)} of {formatCount(run.total)} rows…
              </div>
              <div className="h-1.5 overflow-hidden rounded bg-hover">
                <div className="h-full bg-accent transition-all" style={{ width: `${run.total ? Math.min(100, (run.done / run.total) * 100) : 0}%` }} />
              </div>
            </div>
          )}
          {dry.report && <DryRunReport report={dry.report} />}
        </div>
      )}
    </Modal>
  );
}
