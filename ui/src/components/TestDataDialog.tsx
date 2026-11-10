// "Generate test data…" (explorer table menu): rows made up from the
// table's column types, previewed as an output, and written into the table
// through the load code when asked. Foreign key columns take keys read from
// the parent tables.

import { useEffect, useRef, useState } from "react";
import { AlertTriangle, Eye, Link2, Loader2, Square, Upload } from "lucide-react";
import { api, onLoadProgress, toError } from "../lib/api";
import type { LoadDryRun, LoadReport, TestDataColumn, TestDataOutput, TestDataPlan, TestDataSpec } from "../lib/types";
import { formatCount, uid } from "../lib/util";
import { openOutput } from "../outputs";
import { useStore } from "../store";
import type { TestDataRequest } from "../store/uiSlice";
import { DryRunReport } from "./job/LoadFields";
import { Modal } from "./ui";

export const MAX_TEST_ROWS = 100_000;

/** Columns ticked at first: what DataBrain can fill and the database doesn't. */
export function defaultColumns(cols: TestDataColumn[]): Set<string> {
  return new Set(cols.filter((c) => c.supported && !c.database_fills).map((c) => c.name));
}

/** The spec sent for the ticked columns. */
export function specFor(cols: TestDataColumn[], ticked: Set<string>, rows: number, nullPercent: number, seed: string, parentKeys: boolean): TestDataSpec {
  const s = seed.trim();
  return {
    rows: Math.max(1, Math.min(MAX_TEST_ROWS, Math.floor(rows) || 1)),
    null_percent: Math.max(0, Math.min(100, Math.floor(nullPercent) || 0)),
    seed: s && /^\d+$/.test(s) ? Number(s) : null,
    skip: cols.filter((c) => c.supported && !c.database_fills && !ticked.has(c.name)).map((c) => c.name),
    fill: cols.filter((c) => c.supported && c.database_fills && ticked.has(c.name)).map((c) => c.name),
    parent_keys: parentKeys,
  };
}

/** Preview tabs of earlier generations, per table: replaced by the next one. */
const previews = new Map<string, string>();

export function TestDataDialogHost() {
  const req = useStore((s) => s.testDataDialog);
  const close = useStore((s) => s.openTestDataDialog);
  if (!req) return null;
  return <TestDataDialog req={req} onClose={() => close(null)} />;
}

type Plan = { status: "loading" } | { status: "ready"; plan: TestDataPlan } | { status: "error"; message: string };

function TestDataDialog({ req, onClose }: { req: TestDataRequest; onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const conn = useStore((s) => s.connections.find((c) => c.id === req.connection_id));
  const [plan, setPlan] = useState<Plan>({ status: "loading" });
  const [ticked, setTicked] = useState<Set<string>>(new Set());
  const [rows, setRows] = useState(100);
  const [nulls, setNulls] = useState(10);
  const [seed, setSeed] = useState("");
  const [parentKeys, setParentKeys] = useState(true);
  const [busy, setBusy] = useState<null | "preview" | "dry" | "load">(null);
  const [made, setMade] = useState<TestDataOutput | null>(null);
  const [dry, setDry] = useState<LoadDryRun | null>(null);
  const [progress, setProgress] = useState<{ id: string; done: number; total: number } | null>(null);
  const [report, setReport] = useState<LoadReport | null>(null);
  const loadId = useRef<string | null>(null);

  useEffect(() => {
    let live = true;
    api
      .testDataPlan(req.connection_id, req.schema, req.table)
      .then((p) => {
        if (!live) return;
        setPlan({ status: "ready", plan: p });
        setTicked(defaultColumns(p.columns));
      })
      .catch((e) => live && setPlan({ status: "error", message: toError(e).message }));
    return () => {
      live = false;
    };
  }, [req.connection_id, req.schema, req.table]);

  useEffect(() => {
    const un = onLoadProgress((e) => e.load_id === loadId.current && setProgress((p) => (p ? { ...p, done: e.done, total: e.total } : p)));
    return () => void un.then((f) => f());
  }, []);

  // Settings changed: earlier rows and checks no longer match them.
  const tickedKey = [...ticked].sort().join(",");
  useEffect(() => {
    setMade(null);
    setDry(null);
  }, [rows, nulls, seed, parentKeys, tickedKey]);

  if (plan.status !== "ready") {
    return (
      <Modal title={`Generate test data · ${req.table}`} onClose={onClose}>
        {plan.status === "loading" ? (
          <div className="flex items-center gap-2 text-[12.5px] text-muted">
            <Loader2 size={14} className="animate-spin" /> Reading the table's columns…
          </div>
        ) : (
          <p className="text-[12.5px] text-danger" role="alert">
            {plan.message}
          </p>
        )}
      </Modal>
    );
  }

  const p = plan.plan;
  const readOnly = !!conn?.config.read_only;
  const key = `${req.connection_id}|${p.table}`;
  const nothing = ticked.size === 0;

  /** Rows for the current settings (made once, reused by preview, dry run and load). */
  const generate = async (): Promise<TestDataOutput> => {
    if (made) return made;
    const out = await api.generateTestData(req.connection_id, req.schema, req.table, specFor(p.columns, ticked, rows, nulls, seed, parentKeys));
    // The previous preview of this table was freed: close its tab.
    const old = previews.get(key);
    const st = useStore.getState();
    const oldTab = old && st.tabs.find((t) => t.output_ref === old);
    if (oldTab && old !== out.output.handle) st.closeTab(oldTab.id);
    previews.set(key, out.output.handle);
    setMade(out);
    return out;
  };
  const args = (table: string) => ({ target_connection_id: req.connection_id, table, mode: "append" as const, key_columns: [], create_table: false });

  const preview = async () => {
    setBusy("preview");
    try {
      const out = await generate();
      await openOutput(out.output);
      onClose();
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(null);
    }
  };
  const dryRun = async () => {
    setBusy("dry");
    try {
      const out = await generate();
      setDry(await api.dryRunResultLoad(out.output.result_id, args(out.table)));
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(null);
    }
  };
  const load = async () => {
    setBusy("load");
    try {
      const out = await generate();
      const id = uid();
      loadId.current = id;
      setProgress({ id, done: 0, total: out.output.rows });
      const r = await api.loadResult(out.output.result_id, args(out.table), id);
      setReport(r);
      toast(`Loaded ${formatCount(r.rows)} test rows into ${req.table}`, "success");
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(null);
      setProgress(null);
      loadId.current = null;
    }
  };

  const toggle = (name: string, on: boolean) =>
    setTicked((s) => {
      const n = new Set(s);
      if (on) n.add(name);
      else n.delete(name);
      return n;
    });

  return (
    <Modal
      title={`Generate test data · ${req.table}`}
      onClose={() => !progress && onClose()}
      width={680}
      footer={
        report ? (
          <button className="btn-primary" onClick={onClose}>
            Close
          </button>
        ) : progress ? (
          <button className="btn-ghost mr-auto" onClick={() => void api.cancelLoad(progress.id)}>
            <Square size={12} /> Stop
          </button>
        ) : (
          <>
            <button
              className="btn-ghost mr-auto border border-line"
              disabled={nothing || !!busy || readOnly}
              onClick={() => void dryRun()}
              title={readOnly ? "This connection is read-only" : "Insert up to 1,000 of the rows and roll back (or check each statement), keeping nothing"}
            >
              {busy === "dry" && <Loader2 size={13} className="animate-spin" />} Dry run
            </button>
            <button className="btn-ghost" disabled={nothing || !!busy} onClick={() => void preview()} title="Open the rows in a tab; Load into… there writes them anywhere">
              {busy === "preview" ? <Loader2 size={13} className="animate-spin" /> : <Eye size={13} />} Preview
            </button>
            <button
              className="btn-primary"
              disabled={nothing || !!busy || readOnly}
              onClick={() => void load()}
              title={readOnly ? `Connection "${conn?.name}" is read-only: preview the rows, or load them elsewhere with Load into…` : `Insert the rows into ${p.table}`}
            >
              {busy === "load" ? <Loader2 size={13} className="animate-spin" /> : <Upload size={13} />} Generate and load {formatCount(Math.min(rows, MAX_TEST_ROWS))} rows
            </button>
          </>
        )
      }
    >
      {report ? (
        <div className="space-y-1 text-[12.5px]" role="status">
          <p>
            {formatCount(report.rows)} rows written into <span className="font-mono">{p.table}</span> on {conn?.name}
            {report.transaction ? ", in one transaction" : ""}.
          </p>
          {[...(made?.notes ?? []), ...report.notices].map((n, i) => (
            <p key={i} className="text-[11.5px] text-muted">
              {n}
            </p>
          ))}
        </div>
      ) : (
        <div className="space-y-3 text-[12.5px]">
          {p.foreign_keys.length > 0 && (
            <div className="space-y-1 rounded-md border border-accent/40 bg-accent/8 px-2 py-1.5 text-[11.5px]" role="note">
              <div className="flex items-center gap-1.5 font-medium">
                <Link2 size={12} className="text-accent" /> This table has {p.foreign_keys.length} foreign key{p.foreign_keys.length === 1 ? "" : "s"}
              </div>
              <ul className="space-y-0.5 text-muted">
                {p.foreign_keys.map((f) => (
                  <li key={f.columns.join(",")}>
                    <span className="font-mono">{f.columns.join(", ")}</span> → <span className="font-mono">{f.parent}</span> ({f.ref_columns.join(", ")})
                  </li>
                ))}
              </ul>
              <label className="flex items-center gap-2 pt-0.5">
                <input type="checkbox" checked={parentKeys} onChange={(e) => setParentKeys(e.target.checked)} /> Use existing keys read from the parent tables (up to
                1,000 each)
              </label>
              {!parentKeys && <p className="text-warning">Made-up values will likely break these foreign keys when loaded.</p>}
            </div>
          )}
          {p.warnings
            .filter((w) => !w.includes("foreign key"))
            .map((w) => (
              <p key={w} className="flex items-start gap-1.5 rounded-md bg-warning/10 px-2 py-1.5 text-[11.5px] text-warning">
                <AlertTriangle size={12} className="mt-0.5 shrink-0" /> {w}
              </p>
            ))}
          <div className="grid grid-cols-3 gap-2">
            <label className="space-y-1">
              <span className="block text-[11.5px] font-medium text-muted">Rows</span>
              <input className="field" type="number" min={1} max={MAX_TEST_ROWS} value={rows} onChange={(e) => setRows(Number(e.target.value))} />
            </label>
            <label className="space-y-1">
              <span className="block text-[11.5px] font-medium text-muted">NULLs in nullable columns (%)</span>
              <input className="field" type="number" min={0} max={100} value={nulls} onChange={(e) => setNulls(Number(e.target.value))} />
            </label>
            <label className="space-y-1">
              <span className="block text-[11.5px] font-medium text-muted">Seed (same seed, same rows)</span>
              <input className="field font-mono" placeholder="random" value={seed} onChange={(e) => setSeed(e.target.value.replace(/\D/g, ""))} />
            </label>
          </div>
          <div className="max-h-72 overflow-auto rounded-md border border-line">
            <table className="w-full border-collapse text-[12px]">
              <thead className="sticky top-0 bg-panel text-[11px] text-muted">
                <tr className="border-b border-line">
                  <th className="w-6 px-2 py-1" />
                  <th className="px-2 py-1 text-left font-normal">Column</th>
                  <th className="px-2 py-1 text-left font-normal">Type</th>
                  <th className="px-2 py-1 text-left font-normal">Gets</th>
                </tr>
              </thead>
              <tbody>
                {p.columns.map((c) => (
                  <tr key={c.name} className={`border-b border-line/50 ${c.supported ? "" : "text-muted"}`}>
                    <td className="px-2 py-1">
                      <input
                        type="checkbox"
                        aria-label={`Fill ${c.name}`}
                        disabled={!c.supported}
                        checked={ticked.has(c.name)}
                        onChange={(e) => toggle(c.name, e.target.checked)}
                      />
                    </td>
                    <td className="px-2 py-1 font-mono">
                      {c.name}
                      {c.primary_key && <span className="ml-1 text-[10.5px] text-accent">PK</span>}
                    </td>
                    <td className="px-2 py-1 font-mono text-[11.5px] text-muted">{c.data_type}</td>
                    <td className="px-2 py-1" title={c.note ?? undefined}>
                      {ticked.has(c.name) ? c.generator : c.supported ? "left to the database" : c.generator}
                      {c.note && <div className="text-[11px] text-muted">{c.note}</div>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {readOnly && <p className="text-[11.5px] text-muted">Connection "{conn?.name}" is read-only: you can preview the rows, not load them here.</p>}
          {made && (
            <ul className="space-y-0.5 text-[11.5px] text-muted">
              <li>Made {formatCount(made.output.rows)} rows with seed <span className="font-mono">{made.seed}</span>: enter it to make the same rows again.</li>
              {made.notes.map((n) => (
                <li key={n}>{n}</li>
              ))}
            </ul>
          )}
          {progress && (
            <div className="space-y-1" role="status">
              <div className="flex items-center gap-2 text-[11.5px] text-muted">
                <Loader2 size={12} className="animate-spin" /> Writing {formatCount(progress.done)} of {formatCount(progress.total)} rows…
              </div>
              <div className="h-1.5 overflow-hidden rounded bg-hover">
                <div className="h-full bg-accent transition-all" style={{ width: `${progress.total ? Math.min(100, (progress.done / progress.total) * 100) : 0}%` }} />
              </div>
            </div>
          )}
          {dry && <DryRunReport report={dry} />}
        </div>
      )}
    </Modal>
  );
}
