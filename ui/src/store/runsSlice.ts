import type { StateCreator } from "zustand";
import { announceSshHostKey, api, isTauri, toError } from "../lib/api";
import { computeRunTips, slowStatements } from "../queryTips";
import type { JobEvent, OutputInfo, ParamValue } from "../lib/types";
import { errorRange } from "../lib/util";
import { restoredRun } from "./helpers";
import { isQueryTab, type RunInput, type State, type StatementRun, type TabRun } from "./types";

/**
 * Job events that arrived before the `run_query` response told us the job id.
 * Fast statements (warm session) can finish before the response is delivered;
 * without this buffer their `job_finished` would be dropped and the run would
 * look stuck. Replayed as soon as the run is registered.
 */
let outputsTimer: ReturnType<typeof setTimeout> | undefined;

const earlyEvents = new Map<string, { at: number; events: JobEvent[] }>();
const EARLY_TTL_MS = 60_000;

function bufferEarly(e: JobEvent) {
  const now = Date.now();
  for (const [id, v] of earlyEvents) if (now - v.at > EARLY_TTL_MS) earlyEvents.delete(id);
  const entry = earlyEvents.get(e.job_id) ?? { at: now, events: [] };
  entry.events.push(e);
  earlyEvents.set(e.job_id, entry);
}

/** Test hook. */
export const _earlyEventCount = () => earlyEvents.size;

export interface RunsSlice {
  /** All known outputs (newest first); refreshed on `outputs_changed`. */
  outputs: OutputInfo[];
  refreshOutputs: () => Promise<void>;
  /** Show each tab's last results from the previous session (active tab first). */
  restoreTabOutputs: () => Promise<void>;
  /** Patch an output in runs + list after rename/pin. */
  applyOutput: (o: OutputInfo) => void;
  /** Forget dropped outputs in runs, output tabs and the list. */
  forgetOutputs: (handles: string[]) => void;
  runs: Record<string, TabRun>;
  /** Tabs whose session has an open transaction → the connection it is on. */
  txOpen: Record<string, string>;
  /** Re-read whether the tab's session has an open transaction. */
  refreshTxState: (tabId: string, connectionId: string) => Promise<void>;
  /** COMMIT (true) or ROLLBACK the tab's open transaction. */
  endTransaction: (tabId: string, commit: boolean) => Promise<void>;
  /** Manual commit (false) or auto-commit (true) for a query tab. */
  setAutoCommit: (tabId: string, on: boolean) => void;
  historyVersion: number;
  runTab: (tabId: string, mode: "statement" | "all", input: RunInput) => Promise<void>;
  /** Run SQL under a run key (tab id or notebook cell key `nb:{id}:{cell}`). */
  runSql: (
    runKey: string,
    connectionId: string,
    sql: string,
    base?: number,
    sessionKey?: string,
    outputName?: string | null,
    params?: Record<string, ParamValue>,
    /** Override the row limit (null = every row). */
    rowLimit?: number | null,
  ) => Promise<boolean>;
  cancelTab: (tabId: string) => Promise<void>;
  setActiveStatement: (tabId: string, index: number) => void;
  handleJobEvent: (e: JobEvent) => void;
  /** Index / partition / cluster-key tips for the run's slow statements. */
  computeTips: (runKey: string, jobId: string) => Promise<void>;
}

export const createRunsSlice: StateCreator<State, [], [], RunsSlice> = (set, get) => ({
  outputs: [],
  refreshOutputs: async () => {
    if (!isTauri()) return;
    try {
      const outputs = await api.listOutputs();
      set((s) => {
        // DuckDB explorers list outputs under results.main: drop stale caches.
        const duck = new Set(s.connections.filter((c) => c.config.kind === "duckdb").map((c) => c.id));
        const objects = Object.fromEntries(Object.entries(s.objects).filter(([k]) => !k.endsWith("|results.main")));
        const columns = Object.fromEntries(Object.entries(s.columns).filter(([k]) => !k.includes("|results.main|")));
        const schemas = Object.fromEntries(
          Object.entries(s.schemas).filter(([id, list]) => !(duck.has(id) && outputs.length > 0 && !list.some((x) => x.name === "results.main"))),
        );
        // Catalog-first: the `results` catalog appears with the first output.
        const catalogs = Object.fromEntries(
          Object.entries(s.catalogs).filter(([id, list]) => !(duck.has(id) && outputs.length > 0 && !list.some((x) => x.name === "results"))),
        );
        return { outputs, objects, columns, schemas, catalogs };
      });
      // Reload what the explorer shows open.
      const st = get();
      for (const c of st.connections) {
        if (c.config.kind !== "duckdb") continue;
        if (st.treeOpen[`c|${c.id}`] && !st.catalogs[c.id] && !st.schemas[c.id]) st.openExplorer(c.id).catch(() => {});
        else if (st.treeOpen[`c|${c.id}`] && !st.catalogs[c.id]) st.loadSchemas(c.id).catch(() => {});
        if (st.treeOpen[`s|${c.id}|results.main`] && st.schemas[c.id]?.some((x) => x.name === "results.main")) st.loadObjects(c.id, "results.main").catch(() => {});
      }
    } catch {
      /* backend restarting */
    }
  },
  restoreTabOutputs: async () => {
    const { tabs, activeTabId } = get();
    const order = [...tabs].filter(isQueryTab).sort((a, b) => Number(b.id === activeTabId) - Number(a.id === activeTabId));
    for (const t of order) {
      const outs = get().outputs.filter((o) => o.tab_id === t.id && o.active);
      if (!outs.length || get().runs[t.id]) continue;
      try {
        // Saved results are read from disk on first use.
        const loaded = await Promise.all(outs.map((o) => (o.state === "on_disk" ? api.loadOutput(o.handle) : Promise.resolve(o))));
        const run = restoredRun(t.id, loaded);
        // The user may have run the tab meanwhile.
        if (run && !get().runs[t.id]) set((s) => ({ runs: { ...s.runs, [t.id]: run } }));
      } catch {
        // Snapshot missing or unreadable: the tab just starts empty.
      }
    }
  },
  forgetOutputs: (handles) =>
    set((s) => {
      const gone = new Set(handles);
      const runs = { ...s.runs };
      for (const [k, r] of Object.entries(runs)) {
        if (!r.statements.some((x) => x.output && gone.has(x.output.handle))) continue;
        runs[k] = {
          ...r,
          statements: r.statements.map((x) =>
            x.output && gone.has(x.output.handle) ? { ...x, output: undefined, result: undefined, notices: [...x.notices, `Output ${x.output.handle} was dropped. Run again to see rows.`] } : x,
          ),
        };
      }
      const tabs = s.tabs.filter((t) => !(t.output_ref && gone.has(t.output_ref)));
      const activeTabId = tabs.some((t) => t.id === s.activeTabId) ? s.activeTabId : (tabs[0]?.id ?? null);
      return { runs, tabs, activeTabId, outputs: s.outputs.filter((o) => !gone.has(o.handle)) };
    }),
  applyOutput: (o) =>
    set((s) => {
      const runs = { ...s.runs };
      for (const [k, r] of Object.entries(runs)) {
        if (r.statements.some((x) => x.output?.handle === o.handle)) {
          runs[k] = { ...r, statements: r.statements.map((x) => (x.output?.handle === o.handle ? { ...x, output: o } : x)) };
        }
      }
      return { runs, outputs: s.outputs.map((x) => (x.handle === o.handle ? o : x)) };
    }),
  runs: {},
  txOpen: {},
  refreshTxState: async (tabId, connectionId) => {
    if (!isTauri()) return;
    const open = await api.transactionState(connectionId, tabId).catch(() => false);
    set((s) => {
      const txOpen = { ...s.txOpen };
      if (open) txOpen[tabId] = connectionId;
      else delete txOpen[tabId];
      return { txOpen };
    });
  },
  endTransaction: async (tabId, commit) => {
    const connId = get().txOpen[tabId];
    if (!connId) return;
    try {
      const still = await api.endTransaction(connId, tabId, commit);
      set((s) => {
        const txOpen = { ...s.txOpen };
        if (still) txOpen[tabId] = connId;
        else delete txOpen[tabId];
        return { txOpen };
      });
      get().toast(commit ? "Committed" : "Rolled back", "success");
    } catch (e) {
      get().toast(toError(e).message, "error");
      await get().refreshTxState(tabId, connId);
    }
  },
  setAutoCommit: (tabId, on) => {
    get().updateTab(tabId, { auto_commit: on ? null : false });
    if (isTauri()) api.setAutoCommit(tabId, on).catch(() => {});
  },
  historyVersion: 0,
  runTab: async (tabId, mode, input) => {
    const s = get();
    const tab = s.tabs.find((t) => t.id === tabId);
    if (!isQueryTab(tab)) return;
    if (!s.backendAvailable) {
      s.toast("Queries can only run inside the DataBrain desktop app", "error");
      return;
    }
    const conn = s.connections.find((c) => c.id === tab.connection_id);
    if (!conn) {
      s.toast("Choose a connection for this tab first", "error");
      return;
    }
    if (s.runs[tabId]?.running) return;

    let sql: string;
    let base = 0;
    if (input.selTo > input.selFrom) {
      sql = input.doc.slice(input.selFrom, input.selTo);
      base = input.selFrom;
    } else if (mode === "all") {
      sql = input.doc;
    } else {
      const span = await api.statementAtCursor(conn.config.kind, input.doc, input.cursor);
      if (!span) {
        s.toast("Nothing to run", "info");
        return;
      }
      sql = span.sql;
      base = span.start;
    }
    if (!sql.trim()) {
      s.toast("Nothing to run", "info");
      return;
    }
    // `:name` parameters: every one in the statement needs a value.
    const values = await get().paramsFor(tabId, conn.config.kind, sql);
    if (!values) return;

    await get().runSql(tabId, conn.id, sql, base, undefined, undefined, values);
  },

  runSql: async (runKey, connectionId, sql, base = 0, sessionKey, outputName, params, rowLimit) => {
    const conn = get().connections.find((c) => c.id === connectionId);
    const submit = async (confirmed: boolean): Promise<boolean> => {
      try {
        const resp = await api.runQuery({
          connection_id: connectionId,
          tab_id: runKey,
          sql,
          base_offset: base,
          row_limit: rowLimit !== undefined ? rowLimit : get().rowLimit > 0 ? get().rowLimit : null,
          confirmed,
          session_key: sessionKey ?? null,
          output_name: outputName ?? null,
          params: params ?? {},
        });
        if (resp.status === "needs_confirmation") {
          return await new Promise<boolean>((resolve) =>
            get().askConfirm({
              title: "Confirm execution",
              reasons: resp.reasons,
              confirmLabel: "Run anyway",
              onConfirm: () => void submit(true).then(resolve),
              onCancel: () => resolve(false),
            }),
          );
        }
        set((st) => ({
          runs: {
            ...st.runs,
            [runKey]: {
              jobId: resp.job_id,
              running: true,
              startedAt: Date.now(),
              statements: resp.statements.map((p) => ({ plan: p, status: "pending", notices: [] })),
              activeIndex: null,
              connectionId,
            },
          },
        }));
        const early = earlyEvents.get(resp.job_id);
        if (early) {
          earlyEvents.delete(resp.job_id);
          for (const ev of early.events) get().handleJobEvent(ev);
        }
        return true;
      } catch (e) {
        const err = toError(e);
        if (err.kind === "reauth_required" && conn) {
          // Interactive sign-in expired: sign in, then retry once.
          if (await get().signInConnection(conn.id)) return submit(confirmed);
          return false;
        }
        get().toast(err.message, "error");
        return false;
      }
    };
    return submit(false);
  },
  cancelTab: async (tabId) => {
    const run = get().runs[tabId];
    if (!run?.jobId || !run.running) return;
    const jobId = run.jobId;
    const found = await api.cancelQuery(jobId).catch(() => false);
    if (found) return;
    // The backend no longer knows this job (it already ended, and its final
    // event was lost): mark the run as finished so the UI is never stuck.
    set((s) => {
      const r = s.runs[tabId];
      if (!r || r.jobId !== jobId || !r.running) return {};
      return {
        runs: {
          ...s.runs,
          [tabId]: {
            ...r,
            running: false,
            finishedStatus: "cancelled",
            durationMs: Date.now() - r.startedAt,
            statements: r.statements.map((x) =>
              x.status === "pending" || x.status === "running" ? { ...x, status: "cancelled" } : x,
            ),
          },
        },
      };
    });
  },

  setActiveStatement: (tabId, index) =>
    set((s) => {
      const run = s.runs[tabId];
      if (!run) return {};
      return { runs: { ...s.runs, [tabId]: { ...run, activeIndex: index } } };
    }),

  handleJobEvent: (e) => {
    if (e.type === "outputs_changed") {
      clearTimeout(outputsTimer);
      outputsTimer = setTimeout(() => void get().refreshOutputs(), 120);
      return;
    }
    // Job steps report through "job-run" events (jobsStore).
    if (e.tab_id.startsWith("job:")) return;
    const cur = get().runs[e.tab_id];
    if (!cur || cur.jobId !== e.job_id) {
      // Either stale (older job) or early (response not received yet).
      bufferEarly(e);
      return;
    }
    set((s) => {
      const run = s.runs[e.tab_id];
      if (!run || run.jobId !== e.job_id) return {};
      const statements = run.statements.slice();
      const upd = (i: number, patch: Partial<StatementRun>) => {
        if (statements[i]) statements[i] = { ...statements[i], ...patch };
      };
      let next: TabRun = { ...run, statements };
      switch (e.type) {
        case "statement_started": {
          upd(e.index, { status: "running", startedAt: Date.now() });
          // Still running after the slow-query threshold: look for why now,
          // on another session, instead of waiting for it to end.
          const ms = get().slowQuerySeconds * 1000;
          if (ms > 0) {
            const { tab_id, job_id, index } = e;
            setTimeout(() => {
              const r = get().runs[tab_id];
              if (r?.jobId === job_id && r.statements[index]?.status === "running") void get().computeTips(tab_id, job_id);
            }, ms + 50);
          }
          break;
        }
        case "progress":
          upd(e.index, { progressRows: e.rows });
          break;
        case "downloading": {
          const cur = statements[e.index];
          upd(e.index, { downloading: true, serverMs: e.elapsed_ms, notices: e.notice && cur ? [...cur.notices, e.notice] : (cur?.notices ?? []) });
          break;
        }
        case "statement_finished":
          upd(e.index, {
            status: "done",
            result: e.result ?? undefined,
            output: e.output ?? undefined,
            rowsAffected: e.rows_affected,
            durationMs: e.duration_ms,
            notices: e.notices,
            downloading: false,
            serverMs: e.server_ms ?? statements[e.index]?.serverMs,
          });
          if (e.result) next.activeIndex = e.index;
          break;
        case "statement_failed": {
          const cancelled = e.error.kind === "cancelled";
          if (e.error.code === "oracle_client_missing") setTimeout(() => set({ oracleClientPrompt: true }), 0);
          announceSshHostKey(e.error);
          upd(e.index, {
            status: cancelled ? "cancelled" : "error",
            error: e.error,
            durationMs: e.duration_ms,
          });
          const st = statements[e.index];
          if (!cancelled && st) next.errorRange = errorRange(st.plan, e.error.position);
          if (next.activeIndex === null || !cancelled) next.activeIndex = e.index;
          break;
        }
        case "job_finished":
          next = {
            ...next,
            running: false,
            finishedStatus: e.status,
            durationMs: e.duration_ms,
            statements: statements.map((x) =>
              x.status === "pending" || x.status === "running"
                ? { ...x, status: e.status === "cancelled" ? "cancelled" : x.status }
                : x,
            ),
          };
          if (next.activeIndex === null && statements.length > 0) {
            next.activeIndex = statements.length - 1;
          }
          if (e.status !== "cancelled") setTimeout(() => void get().computeTips(e.tab_id, e.job_id), 0);
          {
            const txOpen = { ...s.txOpen };
            if (e.in_transaction && run.connectionId) txOpen[e.tab_id] = run.connectionId;
            else if (txOpen[e.tab_id] === run.connectionId) delete txOpen[e.tab_id];
            return {
              runs: { ...s.runs, [e.tab_id]: next },
              historyVersion: s.historyVersion + 1,
              txOpen,
            };
          }
      }
      return { runs: { ...s.runs, [e.tab_id]: next } };
    });
  },

  computeTips: async (runKey, jobId) => {
    const run = get().runs[runKey];
    if (!run || run.jobId !== jobId) return;
    const connId = run.connectionId;
    if (!connId) return;
    const conn = get().connections.find((c) => c.id === connId);
    const slow = slowStatements(run.statements, get().slowQuerySeconds * 1000, Date.now());
    if (!slow.length) return; // fast queries get no tips
    try {
      const tips = await computeRunTips(connId, conn?.config.kind, slow);
      set((s) => {
        const cur = s.runs[runKey];
        if (!cur || cur.jobId !== jobId) return {};
        return { runs: { ...s.runs, [runKey]: { ...cur, tips } } };
      });
    } catch {
      // Tips are best effort.
    }
  },
});
