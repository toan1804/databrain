// "Generate test data…" (explorer table menu): rows made up from the
// table's column types, previewed as an output, and written into the table
// through the load code when asked. Foreign key columns take keys read from
// the parent tables. "Generate values with AI" has an AI model write the
// text columns (or every column it may), from the table's structure only.

import { useEffect, useRef, useState } from "react";
import { AlertTriangle, BookOpen, Eye, Link2, Loader2, Sparkles, Square, Upload } from "lucide-react";
import { api, onLoadProgress, toError } from "../lib/api";
import type { LoadDryRun, LoadReport, TestDataColumn, TestDataOutput, TestDataPlan, TestDataSpec } from "../lib/types";
import { formatCount, uid } from "../lib/util";
import { openOutput } from "../outputs";
import { useAi } from "../aiStore";
import { AI_BATCH_ROWS, MAX_AI_ROWS, aiRequest, aiTarget, aiWrites, clampAiRows } from "../lib/testDataAi";
import { PATTERN_HELP, patternOf, patternsFor, patternsKey } from "../lib/testDataPatterns";
import { useStore } from "../store";
import type { TestDataRequest } from "../store/uiSlice";
import { DryRunReport } from "./job/LoadFields";
import { PatternGuide } from "./PatternGuide";
import { Modal } from "./ui";

export const MAX_TEST_ROWS = 100_000;
/** Largest seed: seeds travel as JavaScript numbers, exact up to 2^53 - 1. */
export const MAX_SEED = Number.MAX_SAFE_INTEGER;

/** Why a typed seed can't be used (null: it can, or none was typed). */
export function seedProblem(seed: string): string | null {
  const s = seed.trim();
  if (!s) return null;
  if (!/^\d+$/.test(s)) return "A seed is a whole number";
  return BigInt(s) > BigInt(MAX_SEED) ? `Choose a seed up to ${MAX_SEED}` : null;
}

/** Columns ticked at first: what DataBrain can fill and the database doesn't. */
export function defaultColumns(cols: TestDataColumn[]): Set<string> {
  return new Set(cols.filter((c) => c.supported && !c.database_fills).map((c) => c.name));
}

/** The spec sent for the ticked columns. */
export function specFor(
  cols: TestDataColumn[],
  ticked: Set<string>,
  rows: number,
  nullPercent: number,
  seed: string,
  parentKeys: boolean,
  patterns: Record<string, string> = {},
): TestDataSpec {
  const s = seed.trim();
  return {
    rows: Math.max(1, Math.min(MAX_TEST_ROWS, Math.floor(rows) || 1)),
    null_percent: Math.max(0, Math.min(100, Math.floor(nullPercent) || 0)),
    // Only seeds that survive a JavaScript number (else the rows wouldn't repeat).
    seed: s && seedProblem(s) === null ? Number(s) : null,
    skip: cols.filter((c) => c.supported && !c.database_fills && !ticked.has(c.name)).map((c) => c.name),
    fill: cols.filter((c) => c.supported && c.database_fills && !c.computed && ticked.has(c.name)).map((c) => c.name),
    parent_keys: parentKeys,
    patterns: patternsFor(cols, patterns, ticked),
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
  const aiSettings = useStore((s) => s.testDataAi);
  const setAiSettings = useStore((s) => s.setTestDataAi);
  const providers = useAi((s) => s.providers);
  const panelProvider = useAi((s) => s.providerId);
  const panelModel = useAi((s) => s.model);
  const [aiRun, setAiRun] = useState<string | null>(null);
  // Column patterns: the user's and the AI's, over each column's default.
  const pKey = patternsKey(req.connection_id, req.schema, req.table);
  const storedPatterns = useStore((s) => s.testDataPatterns[pKey]);
  const savePatterns = useStore((s) => s.setTestDataPatterns);
  const [patterns, setPatterns] = useState<Record<string, string>>(() => storedPatterns ?? {});
  // The patterns as last rendered, for code that runs after an await.
  const patternsNow = useRef(patterns);
  patternsNow.current = patterns;
  const [suggesting, setSuggesting] = useState(false);
  const [guide, setGuide] = useState(false);
  // The pattern field last clicked: where the guide's examples go.
  const [patternField, setPatternField] = useState<string | null>(null);
  // The AI rows field as typed (committed on blur, so it can be cleared).
  const [aiRowsText, setAiRowsText] = useState(String(aiSettings.rows));
  const running = useRef<string | null>(null);
  // Closing the dialog stops a request still running.
  useEffect(
    () => () => {
      if (running.current) void api.aiCancel(running.current);
    },
    [],
  );

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
  }, [rows, nulls, seed, parentKeys, tickedKey, aiSettings, panelProvider, panelModel, conn?.ai_policy, patterns]);

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
  const badSeed = seedProblem(seed);
  const nothing = ticked.size === 0 || !!badSeed;
  // The AI: allowed on this connection, and a model provider set up.
  const policy = conn?.ai_policy;
  const target = aiTarget(providers, { providerId: panelProvider, model: panelModel }, policy?.allowed_providers ?? []);
  const aiBlocked =
    policy && !policy.ai_enabled
      ? "AI is turned off for this connection (Connection → AI)."
      : policy && !policy.share_metadata
        ? "This connection doesn't share table structure with the AI (Connection → AI → Share schema metadata)."
        : !target
          ? policy?.allowed_providers.length
            ? "None of the AI providers this connection allows is set up."
            : "Add an AI provider in the AI panel to use this."
          : null;
  const ai = { ...aiSettings, enabled: aiSettings.enabled && !aiBlocked };
  const aiColumns = p.columns.filter((c) => aiWrites(c, ticked, ai));
  const aiRows = Math.min(clampAiRows(ai.rows), Math.max(1, Math.floor(rows) || 1));
  const aiCalls = Math.ceil(aiRows / AI_BATCH_ROWS);
  const providerName = providers.find((x) => x.id === target?.provider_id)?.name ?? "the AI";

  const spec = () => specFor(p.columns, ticked, rows, nulls, seed, parentKeys, patterns);
  const setPattern = (col: string, value: string) => setPatterns((m) => ({ ...m, [col]: value }));
  const keepPatterns = () => savePatterns(pKey, patterns);
  /** The AI's patterns for the ticked columns, over the current ones. */
  const suggest = async () => {
    if (!target) return;
    const run = uid();
    running.current = run;
    setAiRun(run);
    setSuggesting(true);
    try {
      const got = await api.suggestTestDataPatterns(req.connection_id, req.schema, req.table, spec(), { ...target, all_columns: true, rows: 1 }, run);
      // Over the patterns as they are now: edits typed while the AI ran stay.
      setPatterns((cur) => ({ ...cur, ...got }));
      savePatterns(pKey, { ...patternsNow.current, ...got });
      const n = Object.keys(got).length;
      toast(n ? `${providerName} suggested ${n} pattern${n === 1 ? "" : "s"}: check them in the Pattern column` : `${providerName} had no pattern to suggest`, "info");
    } catch (e) {
      failed(e);
    } finally {
      running.current = null;
      setAiRun(null);
      setSuggesting(false);
    }
  };
  /** Rows for the current settings (made once, reused by preview, dry run and load). */
  const generate = async (): Promise<TestDataOutput> => {
    if (made) return made;
    const askAi = aiRequest(ai, target, aiColumns.length);
    const run = askAi ? uid() : null;
    running.current = run;
    setAiRun(run);
    let out: TestDataOutput;
    try {
      out = await api.generateTestData(req.connection_id, req.schema, req.table, spec(), askAi, run);
    } finally {
      running.current = null;
      setAiRun(null);
    }
    // The previous preview of this table was freed: close its tab.
    const old = previews.get(key);
    const st = useStore.getState();
    const oldTab = old && st.tabs.find((t) => t.output_ref === old);
    if (oldTab && old !== out.output.handle) st.closeTab(oldTab.id);
    previews.set(key, out.output.handle);
    setMade(out);
    return out;
  };
  /** An error toast, or a quiet note when the user stopped the AI. */
  const failed = (e: unknown) => {
    const err = toError(e);
    if (err.kind === "cancelled") toast("Stopped generating test data", "info");
    else toast(err.message, "error");
  };
  const args = (table: string) => ({ target_connection_id: req.connection_id, table, mode: "append" as const, key_columns: [], create_table: false });

  const preview = async () => {
    setBusy("preview");
    try {
      const out = await generate();
      await openOutput(out.output);
      onClose();
    } catch (e) {
      failed(e);
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
      failed(e);
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
      failed(e);
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
      width={820}
      footer={
        report ? (
          <button className="btn-primary" onClick={onClose}>
            Close
          </button>
        ) : progress ? (
          <button className="btn-ghost mr-auto" onClick={() => void api.cancelLoad(progress.id)}>
            <Square size={12} /> Stop
          </button>
        ) : aiRun ? (
          <>
            <span className="mr-auto flex items-center gap-1.5 text-[12px] text-muted" role="status">
              <Loader2 size={13} className="animate-spin" /> {suggesting ? `${providerName} is suggesting patterns…` : `${providerName} is writing ${formatCount(aiRows)} rows…`}
            </span>
            <button className="btn-ghost border border-line" onClick={() => void api.aiCancel(aiRun)}>
              <Square size={12} /> Stop
            </button>
          </>
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
              <input
                className="field font-mono"
                placeholder="random"
                inputMode="numeric"
                maxLength={16}
                aria-invalid={!!badSeed}
                value={seed}
                onChange={(e) => setSeed(e.target.value.replace(/\D/g, ""))}
              />
              {badSeed && (
                <span className="block text-[11px] text-danger" role="alert">
                  {badSeed}
                </span>
              )}
            </label>
          </div>
          <div className="space-y-1 rounded-md border border-line px-2 py-1.5 text-[11.5px]">
            <label className="flex items-center gap-2 font-medium">
              <input
                type="checkbox"
                checked={ai.enabled}
                disabled={!!aiBlocked}
                onChange={(e) => setAiSettings({ ...aiSettings, enabled: e.target.checked })}
              />
              <Sparkles size={12} className="text-accent" /> Generate values with AI
            </label>
            {aiBlocked ? (
              <p className="pl-5 text-muted">{aiBlocked}</p>
            ) : (
              ai.enabled && (
                <div className="space-y-1 pl-5">
                  <label className="flex items-center gap-2">
                    <input type="checkbox" checked={ai.textOnly} onChange={(e) => setAiSettings({ ...aiSettings, textOnly: e.target.checked })} /> Only text columns
                    (cheaper; untick to have the AI write every column it can)
                  </label>
                  <label className="flex flex-wrap items-center gap-2">
                    The AI writes
                    <input
                      className="field w-20"
                      type="number"
                      min={1}
                      max={MAX_AI_ROWS}
                      aria-label="Rows the AI writes"
                      value={aiRowsText}
                      onChange={(e) => setAiRowsText(e.target.value.replace(/\D/g, ""))}
                      onBlur={() => {
                        const n = clampAiRows(Number(aiRowsText));
                        setAiRowsText(String(n));
                        if (n !== aiSettings.rows) setAiSettings({ ...aiSettings, rows: n });
                      }}
                      onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
                    />
                    rows; more rows reuse them.
                    <span className="text-muted">
                      {aiColumns.length === 0
                        ? "No ticked column for the AI: everything is made up as usual."
                        : `${aiCalls} call${aiCalls === 1 ? "" : "s"} to ${providerName} for ${aiColumns.length} column${aiColumns.length === 1 ? "" : "s"}.`}
                    </span>
                  </label>
                  <p className="text-muted">
                    Only the table's structure is sent, no rows. Keys and foreign keys never come from the AI, and a value that doesn't fit its column (type, length,
                    allowed values, uniqueness) is made up instead. The seed repeats only the other columns.
                  </p>
                </div>
              )
            )}
          </div>
          <div className="flex items-center gap-2 text-[11.5px] text-muted">
            <span className="mr-auto">A pattern sets the rule a column's values follow, such as 18..90 for an age or %@gmail.com for an email.</span>
            <button className="btn-ghost border border-line py-0.5 text-[11.5px]" aria-expanded={guide} onClick={() => setGuide((g) => !g)}>
              <BookOpen size={12} /> {guide ? "Hide pattern guide" : "Pattern guide"}
            </button>
            <button
              className="btn-ghost border border-line py-0.5 text-[11.5px]"
              disabled={!!aiBlocked || !!busy || !!aiRun || ticked.size === 0}
              title={aiBlocked ?? "Ask the AI for patterns that make the ticked columns realistic (only the table's structure is sent)"}
              onClick={() => void suggest()}
            >
              {suggesting ? <Loader2 size={12} className="animate-spin" /> : <Sparkles size={12} className="text-accent" />} Suggest patterns with AI
            </button>
          </div>
          {guide && (
            <PatternGuide
              target={patternField}
              onPick={(pattern) => {
                if (!patternField) return;
                const next = { ...patterns, [patternField]: pattern };
                setPatterns(next);
                savePatterns(pKey, next);
              }}
            />
          )}
          <div className="max-h-72 overflow-auto rounded-md border border-line">
            <table className="w-full border-collapse text-[12px]">
              <thead className="sticky top-0 bg-panel text-[11px] text-muted">
                <tr className="border-b border-line">
                  <th className="w-6 px-2 py-1" />
                  <th className="px-2 py-1 text-left font-normal">Column</th>
                  <th className="px-2 py-1 text-left font-normal">Type</th>
                  <th className="px-2 py-1 text-left font-normal">Gets</th>
                  <th className="px-2 py-1 text-left font-normal" title={PATTERN_HELP}>
                    Pattern
                  </th>
                </tr>
              </thead>
              <tbody>
                {p.columns.map((c) => (
                  <tr key={c.name} className={`border-b border-line/50 ${c.supported ? "" : "text-muted"}`}>
                    <td className="px-2 py-1">
                      <input
                        type="checkbox"
                        aria-label={`Fill ${c.name}`}
                        disabled={!c.supported || c.computed}
                        title={c.computed ? (c.note ?? "Computed by the database: it can't be filled") : undefined}
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
                      {aiWrites(c, ticked, ai) ? (
                        <span className="inline-flex items-center gap-1">
                          <Sparkles size={11} className="text-accent" aria-label="Written by AI" /> AI: {c.generator}
                        </span>
                      ) : ticked.has(c.name) ? (
                        c.generator
                      ) : c.supported ? (
                        "left to the database"
                      ) : (
                        c.generator
                      )}
                      {c.note && <div className="text-[11px] text-muted">{c.note}</div>}
                    </td>
                    <td className="px-2 py-1">
                      {c.pattern_hint && (
                        <input
                          className="field w-36 py-0.5 font-mono text-[11.5px]"
                          aria-label={`Pattern for ${c.name}`}
                          placeholder={`e.g. ${c.pattern_hint}`}
                          title={PATTERN_HELP}
                          disabled={!ticked.has(c.name)}
                          value={patternOf(c, patterns)}
                          onChange={(e) => setPattern(c.name, e.target.value)}
                          onFocus={() => setPatternField(c.name)}
                          onBlur={keepPatterns}
                        />
                      )}
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
