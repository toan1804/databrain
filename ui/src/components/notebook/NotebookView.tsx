// Notebook tab: ordered SQL / Markdown cells bound to a connection.
//
// Every SQL cell runs under its own run key (`nb:{notebook}:{cell}`) so it
// keeps its own results, while all cells share one database session
// (`nb:{notebook}`), so temp tables and session settings carry across cells.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Code2,
  FileText,
  Loader2,
  PlayCircle,
  Plus,
  Square,
  Type,
  Workflow,
} from "lucide-react";
import { api, isTauri, toError } from "../../lib/api";
import type {
  CellKind,
  Notebook as NotebookT,
  NotebookCell,
} from "../../lib/types";
import { uid } from "../../lib/util";
import { useStore } from "../../store";
import { useJobs } from "../../jobsStore";
import { editorBridge } from "../../editorBridge";
import { ParamBar } from "../ParamBar";
import { paramNames, textualNames } from "../paramsExt";
import { ConnDot, EnvBadge } from "../ui";
import { OutputQueryTarget } from "../OutputChip";
import { resultsConnection } from "../../outputs";
import { cellDeps, referencedOutputs } from "../../lib/dataflow";
import { Cell } from "./Cell";
import { cellKey, waitForRun } from "./shared";

const SAVE_DELAY = 700;

export function NotebookView({
  tabId,
  notebookId,
  visible,
}: {
  tabId: string;
  notebookId: string;
  visible: boolean;
}) {
  const [nb, setNb] = useState<NotebookT | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState<"idle" | "pending" | "saving">("idle");
  const [focused, setFocused] = useState<string | null>(null);
  const [runningAll, setRunningAll] = useState(false);
  const stopAll = useRef(false);
  const saveTimer = useRef<ReturnType<typeof setTimeout>>();
  const latest = useRef<NotebookT | null>(null);
  const connections = useStore((s) => s.connections);
  const updateTab = useStore((s) => s.updateTab);
  const toast = useStore((s) => s.toast);
  // Subscribed (not read once) so Run all / Stop always reflects the cells' state.
  const anyRunning = useStore((s) =>
    Object.entries(s.runs).some(
      ([k, r]) => r.running && k.startsWith(`nb:${notebookId}:`),
    ),
  );

  useEffect(() => {
    let alive = true;
    api
      .getNotebook(notebookId)
      .then((n) => {
        if (!alive) return;
        setNb(n);
        latest.current = n;
      })
      .catch((e) => alive && setError(toError(e).message));
    return () => {
      alive = false;
    };
  }, [notebookId]);

  const flush = useCallback(async () => {
    clearTimeout(saveTimer.current);
    const cur = latest.current;
    if (!cur) return;
    setSaving("saving");
    try {
      await api.saveNotebook(cur);
      setSaving("idle");
      void useStore.getState().refreshNotebooks();
    } catch (e) {
      setSaving("idle");
      toast(`Notebook not saved: ${toError(e).message}`, "error");
    }
  }, [toast]);

  // Tab closed: save, then release the shared session and cell results.
  useEffect(
    () => () => {
      void flush();
      void api.closeTab(`nb:${notebookId}`).catch(() => {});
      for (const c of latest.current?.cells ?? [])
        void api.closeTab(cellKey(notebookId, c.id)).catch(() => {});
    },
    [flush, notebookId],
  );

  const update = useCallback(
    (fn: (n: NotebookT) => NotebookT) => {
      setNb((prev) => {
        if (!prev) return prev;
        const next = fn(prev);
        latest.current = next;
        return next;
      });
      setSaving("pending");
      clearTimeout(saveTimer.current);
      saveTimer.current = setTimeout(() => void flush(), SAVE_DELAY);
    },
    [flush],
  );

  const updateCell = useCallback(
    (id: string, patch: Partial<NotebookCell>) =>
      update((n) => ({
        ...n,
        cells: n.cells.map((c) => (c.id === id ? { ...c, ...patch } : c)),
      })),
    [update],
  );

  // Record last-run summaries when cell jobs finish (subscribed once the notebook has loaded).
  const loaded = nb !== null;
  useEffect(() => {
    if (!loaded) return;
    return useStore.subscribe((s, prev) => {
      for (const c of latest.current?.cells ?? []) {
        const k = cellKey(notebookId, c.id);
        const r = s.runs[k];
        const p = prev.runs[k];
        if (r && p?.running && !r.running) {
          const failed = r.statements.find((x) => x.status === "error");
          const last = [...r.statements].reverse().find((x) => x.result);
          updateCell(c.id, {
            last_run: {
              finished_at: Date.now(),
              duration_ms: r.durationMs ?? 0,
              rows: last?.result?.total_rows ?? null,
              error: failed?.error?.message ?? null,
            },
          });
        }
      }
    });
  }, [loaded, notebookId, updateCell]);

  const connId = nb?.connection_id ?? null;
  const conn = connections.find((c) => c.id === connId);
  const outputNames = useStore((s) =>
    s.outputs
      .filter((o) => o.state !== "evicted")
      .map((o) => o.name ?? o.handle)
      .join("\u0000"),
  );
  const deps = useMemo(
    () =>
      nb
        ? cellDeps(nb.cells, outputNames ? outputNames.split("\u0000") : [])
        : {},
    [nb, outputNames],
  );
  const runDependents = async (id: string) => {
    const order = latest.current?.cells ?? [];
    const seen = new Set<string>();
    const walk = (x: string) => {
      for (const d of deps[x]?.dependents ?? [])
        if (!seen.has(d)) {
          seen.add(d);
          walk(d);
        }
    };
    walk(id);
    for (const c of order) {
      if (!seen.has(c.id)) continue;
      const ok = await runCell(c);
      if (
        !ok ||
        useStore.getState().runs[cellKey(notebookId, c.id)]?.finishedStatus !==
          "success"
      )
        break;
    }
  };

  // Cells that read results.<name> default to the local Results (DuckDB)
  // connection: that is where they run, so the editor's completion, the AI
  // and the cell's connection label use it too.
  const readsOutputs = nb?.cells.some((c) => c.kind === "sql" && !c.connection_id && (deps[c.id]?.reads.length ?? 0) > 0) ?? false;
  const [resultsId, setResultsId] = useState<string | null>(null);
  useEffect(() => {
    if (readsOutputs && !resultsId) resultsConnection().then(setResultsId).catch(() => {});
  }, [readsOutputs, resultsId]);
  const cellConn = (c: NotebookCell) => {
    if (c.connection_id) return c.connection_id;
    if (c.kind === "sql" && (deps[c.id]?.reads.length ?? 0) > 0 && conn?.config.kind !== "duckdb" && resultsId) return resultsId;
    return connId;
  };

  const runCell = useCallback(
    async (c: NotebookCell, sqlOverride?: string) => {
      if (c.kind !== "sql") return false;
      const cid = c.connection_id ?? latest.current?.connection_id;
      if (!cid) {
        toast("Choose a connection for this notebook first", "error");
        return false;
      }
      const sql =
        sqlOverride ??
        editorBridge.get(cellKey(notebookId, c.id))?.state.doc.toString() ??
        c.source;
      if (!sql.trim()) return true;
      // Cells that read results.<name> run on the local Results connection
      // unless their connection is DuckDB already.
      let target = cid;
      const kind = useStore.getState().connections.find((x) => x.id === cid)
        ?.config.kind;
      if (referencedOutputs(sql).length > 0 && kind !== "duckdb") {
        try {
          target = await resultsConnection();
        } catch (e) {
          toast(toError(e).message, "error");
          return false;
        }
      }
      const name = c.output_name?.trim() || null;
      // `:name` parameters come from the bar at the top of the notebook.
      const params = await useStore.getState().paramsFor(tabId, kind ?? "postgres", sql);
      if (!params) return false;
      const ok = await useStore
        .getState()
        .runSql(
          cellKey(notebookId, c.id),
          target,
          sql,
          0,
          `nb:${notebookId}`,
          name,
          params,
        );
      if (ok) await waitForRun(cellKey(notebookId, c.id));
      return ok;
    },
    [notebookId, tabId, toast],
  );

  // Parameters of every SQL cell, for the bar at the top (debounced).
  // Each cell is scanned on its own (an unclosed quote in one cell must not hide the next one's).
  const sqlSources = JSON.stringify(nb?.cells.filter((c) => c.kind === "sql" && (c.source.includes(":") || c.source.includes("&"))).map((c) => c.source) ?? []);
  const nbKind = conn?.config.kind;
  useEffect(() => {
    if (!isTauri()) return;
    const t = setTimeout(() => {
      const sources: string[] = JSON.parse(sqlSources);
      Promise.all(sources.map((src) => api.sqlParameters(nbKind ?? "postgres", src)))
        .then((all) => useStore.getState().setParamNames(tabId, paramNames(all.flat()), textualNames(all.flat())))
        .catch(() => {});
    }, 250);
    return () => clearTimeout(t);
  }, [sqlSources, nbKind, tabId]);

  const runAll = async (fromIndex = 0) => {
    if (!latest.current) return;
    setRunningAll(true);
    stopAll.current = false;
    try {
      for (const c of latest.current.cells.slice(fromIndex)) {
        if (stopAll.current) break;
        if (c.kind !== "sql" || !c.source.trim()) continue;
        const ok = await runCell(c);
        const run = useStore.getState().runs[cellKey(notebookId, c.id)];
        if (!ok || run?.finishedStatus !== "success") break; // stop at the first failure
      }
    } finally {
      setRunningAll(false);
    }
  };

  const stop = () => {
    stopAll.current = true;
    for (const c of latest.current?.cells ?? [])
      void useStore.getState().cancelTab(cellKey(notebookId, c.id));
  };

  const addCell = (kind: CellKind, after?: string, source = "") => {
    const cell: NotebookCell = { id: uid(), kind, source };
    update((n) => {
      const i = after
        ? n.cells.findIndex((c) => c.id === after)
        : n.cells.length - 1;
      const cells = n.cells.slice();
      cells.splice(i + 1, 0, cell);
      return { ...n, cells };
    });
    setFocused(cell.id);
    setTimeout(() => editorBridge.focus(cellKey(notebookId, cell.id)), 30);
    return cell;
  };

  const removeCell = (id: string) => {
    void api.closeTab(cellKey(notebookId, id)).catch(() => {});
    update((n) => ({ ...n, cells: n.cells.filter((c) => c.id !== id) }));
  };

  const moveCell = (id: string, d: -1 | 1) =>
    update((n) => {
      const i = n.cells.findIndex((c) => c.id === id);
      const j = i + d;
      if (i < 0 || j < 0 || j >= n.cells.length) return n;
      const cells = n.cells.slice();
      [cells[i], cells[j]] = [cells[j], cells[i]];
      return { ...n, cells };
    });

  /** ⇧↵: run and move to the next cell (adds one at the end). */
  const runAndAdvance = async (c: NotebookCell) => {
    const cells = latest.current?.cells ?? [];
    const i = cells.findIndex((x) => x.id === c.id);
    const next = cells[i + 1];
    if (next) {
      setFocused(next.id);
      editorBridge.focus(cellKey(notebookId, next.id));
    } else addCell("sql", c.id);
    await runCell(c);
  };

  const setConnection = (id: string | null) => {
    void api.closeTab(`nb:${notebookId}`).catch(() => {}); // new connection → new shared session
    update((n) => ({ ...n, connection_id: id }));
  };

  const rename = (name: string) => {
    if (!name.trim()) return;
    update((n) => ({ ...n, name: name.trim() }));
    updateTab(tabId, { title: name.trim() });
  };

  if (error)
    return (
      <div
        className="p-6 text-[13px] text-danger"
        style={{ display: visible ? "block" : "none" }}
      >
        {error}
      </div>
    );
  if (!nb)
    return (
      <div
        className="flex h-full items-center justify-center text-muted"
        style={{ display: visible ? "flex" : "none" }}
      >
        <Loader2 size={18} className="animate-spin" />
      </div>
    );

  return (
    <div
      className="flex h-full min-h-0 flex-col"
      style={{ display: visible ? "flex" : "none" }}
    >
      <div
        className={`@container flex h-10 shrink-0 items-center gap-2 overflow-x-auto overflow-y-hidden border-b px-2 [&>*]:shrink-0 [&_button]:whitespace-nowrap ${conn?.env === "prod" ? "border-danger/50 bg-danger/5" : "border-line"}`}
      >
        <FileText size={14} className="text-muted" />
        <input
          className="w-44 rounded bg-transparent px-1 text-[13px] font-medium outline-none hover:bg-hover focus:bg-panel-2"
          defaultValue={nb.name}
          key={nb.id}
          aria-label="Notebook name"
          onBlur={(e) => rename(e.target.value)}
          onKeyDown={(e) =>
            e.key === "Enter" && (e.target as HTMLInputElement).blur()
          }
        />
        <div className="flex items-center gap-1.5 rounded-md border border-line bg-panel-2 pl-2">
          <ConnDot color={conn?.color} connected={conn?.connected} />
          <select
            className="h-7 max-w-[200px] bg-transparent pr-1 text-[12.5px] outline-none"
            aria-label="Notebook connection"
            value={connId ?? ""}
            onChange={(e) => setConnection(e.target.value || null)}
          >
            <option value="">No connection</option>
            {connections.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
        </div>
        {conn && <EnvBadge env={conn.env} />}
        <div className="mx-1 h-5 w-px bg-line" />
        {runningAll || anyRunning ? (
          <button className="btn-danger py-1" onClick={stop} title="Stop" aria-label="Stop">
            <Square size={12} fill="currentColor" /> <span className="@max-[640px]:hidden">Stop</span>
          </button>
        ) : (
          <button
            className="btn-primary py-1"
            onClick={() => void runAll()}
            disabled={!conn}
            title="Run all cells in order (stops at the first error)"
            aria-label="Run all"
          >
            <PlayCircle size={14} /> <span className="@max-[640px]:hidden">Run all</span>
          </button>
        )}
        <button
          className="btn-ghost py-1"
          onClick={() => addCell("sql", focused ?? undefined)}
          title="Add a SQL cell"
          aria-label="Add a SQL cell"
        >
          <Code2 size={13} /> <span className="@max-[640px]:hidden">SQL</span>
        </button>
        <button
          className="btn-ghost py-1"
          onClick={() => addCell("markdown", focused ?? undefined)}
          title="Add a text cell"
          aria-label="Add a text cell"
        >
          <Type size={13} /> <span className="@max-[640px]:hidden">Text</span>
        </button>
        <button
          className="btn-ghost py-1"
          disabled={!nb.cells.some((c) => c.kind === "sql" && c.source.trim())}
          title="Create a job with one step per SQL cell, each running after the cell above"
          aria-label="Create job"
          onClick={() => latest.current && void useJobs.getState().createFromNotebook(latest.current)}
        >
          <Workflow size={13} /> <span className="@max-[640px]:hidden">Create job</span>
        </button>
        <span className="ml-auto whitespace-nowrap pl-2 text-[11.5px] text-muted" aria-live="polite">
          {saving === "saving"
            ? "Saving…"
            : saving === "pending"
              ? "Edited"
              : "Saved"}
        </span>
      </div>
      <ParamBar tabId={tabId} kind={conn?.config.kind} />

      <div className="min-h-0 flex-1 overflow-auto bg-bg/40 px-4 py-4">
        <div className="mx-auto max-w-[1100px] space-y-3">
          {nb.cells.map((c, i) => (
            // "Query with SQL" on an output in this cell adds a cell below it.
            <OutputQueryTarget.Provider
              key={c.id}
              value={(sql) => void addCell("sql", c.id, sql)}
            >
              <Cell
                key={c.id}
                nbId={notebookId}
                cell={c}
                index={i}
                total={nb.cells.length}
                connectionId={cellConn(c)}
                notebookConnection={connId}
                focused={focused === c.id}
                onFocus={() => setFocused(c.id)}
                onChange={(patch) => updateCell(c.id, patch)}
                onRun={() => void runCell(c)}
                onRunAdvance={() => void runAndAdvance(c)}
                onRunFromHere={() => void runAll(i)}
                onDelete={() => removeCell(c.id)}
                onMove={(d) => moveCell(c.id, d)}
                onAddBelow={(k) => addCell(k, c.id)}
                deps={deps[c.id]}
                cellIndexOf={(id) => nb.cells.findIndex((x) => x.id === id) + 1}
                onRunDependents={() => void runDependents(c.id)}
              />
            </OutputQueryTarget.Provider>
          ))}
          <div className="flex justify-center gap-2 pt-1 pb-10">
            <button
              className="btn-ghost border border-dashed border-line py-1"
              onClick={() => addCell("sql")}
            >
              <Plus size={13} /> SQL cell
            </button>
            <button
              className="btn-ghost border border-dashed border-line py-1"
              onClick={() => addCell("markdown")}
            >
              <Plus size={13} /> Text cell
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
