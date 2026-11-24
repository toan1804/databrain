import type { StateCreator } from "zustand";
import { announcePasswordExpired, announceSshHostKey, api, isTauri, toError } from "../lib/api";
import { computeRunTips, slowStatements } from "../queryTips";
import type { JobEvent, Origin, OutputInfo, ParamValue } from "../lib/types";
import { errorRange } from "../lib/util";
import { outputLabel } from "../lib/dataflow";
import { ddlEffect } from "./explorerSlice";
import { keptIndex, outputRun, persistAdded, persistTabs, restoredRun } from "./helpers";
import { uid } from "../lib/util";
import { isQueryTab, type AddedResult, type RunInput, type State, type StatementRun, type TabRun } from "./types";

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

/**
 * Run keys whose owner was removed for good (`retireRunKeys`: a notebook
 * cell deleted). Cells live in their notebook's view, not in the store, so
 * `runKeyOpen` can't see one go; a run still starting for such a key is
 * stopped when its answer comes back. Keys are unique ids, never reused.
 */
const retiredKeys = new Set<string>();

/**
 * Whether what `key` runs for is still open: a query tab (its id), a
 * notebook's cell (`nb:{notebook}:{cell}`) or a dashboard's chart
 * (`dash:{dashboard}:{chart}`), whose notebook or dashboard is in a tab.
 */
function runKeyOpen(s: State, key: string): boolean {
  const [kind, id, part] = key.split(":");
  if (kind === "nb" && id) return s.tabs.some((t) => t.notebook_id === id);
  // A dashboard's chart: its dashboard open and the chart still on it.
  if (kind === "dash" && id) return s.tabs.some((t) => t.dashboard_id === id) && (!part || !!s.dashboards.find((d) => d.id === id)?.charts.some((c) => c.id === part));
  return s.tabs.some((t) => t.id === key);
}

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
  /** Drop the runs of closed run keys (notebook cells removed or closed); `prefix` drops every key starting with it. */
  forgetRuns: (keys: string[], prefix?: string) => void;
  /** Run keys whose owner is gone for good (a notebook cell deleted): their runs go, and a run of one still starting is stopped. */
  retireRunKeys: (keys: string[]) => void;
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
    /** Run it as a new version of this output only (saved grid edits shown
     * again): the run's other results stay. `sql` is that output's statement. */
    replaces?: string,
    /** Who runs it, for the history (default: the user); `explorer` runs one read only. */
    origin?: Origin,
  ) => Promise<boolean>;
  /** Run the statement of result `index` of the run again, as that result only. */
  refreshResult: (runKey: string, index: number) => Promise<boolean>;
  cancelTab: (tabId: string) => Promise<void>;
  setActiveStatement: (tabId: string, index: number) => void;
  /** Keep a result in the tab when it runs again, or let go of a kept one. */
  keepResult: (tabId: string, output: OutputInfo, kept: boolean) => Promise<void>;
  /** Results added to each tab from elsewhere (+ in its result tabs), remembered across restarts. */
  addedResults: Record<string, AddedResult[]>;
  /** Show another tab's result in this tab, read-only, while its own tab keeps it (nothing is pinned or saved). */
  addResult: (tabId: string, output: OutputInfo) => Promise<void>;
  /** Close a result added to the tab. */
  removeAddedResult: (tabId: string, handle: string) => void;
  /** A tab closed: its added results are forgotten. */
  dropAddedResults: (tabId: string) => void;
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
        // Added results last as long as their own tab keeps the output.
        const live = new Set(outputs.filter((o) => o.state !== "evicted").map((o) => o.handle));
        let gone: Partial<State> = {};
        for (const [tabId, refs] of Object.entries(s.addedResults)) {
          for (const r of refs) if (!live.has(r.handle)) gone = { ...gone, ...withoutAdded({ ...s, ...gone } as State, tabId, r.handle) };
        }
        return { outputs, objects, columns, schemas, catalogs, ...gone };
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
    const load = (o: OutputInfo) => (o.state === "on_disk" ? api.loadOutput(o.handle) : Promise.resolve(o));
    for (const t of order) {
      const outs = get().outputs.filter((o) => o.tab_id === t.id && o.active);
      // Results added from other tabs that still exist.
      const added = (get().addedResults[t.id] ?? []).map((a) => get().outputs.find((o) => o.handle === a.handle && o.state !== "evicted")).filter((o): o is OutputInfo => !!o);
      if ((!outs.length && !added.length) || get().runs[t.id]) continue;
      try {
        // Saved results are read from disk on first use.
        const loaded = await Promise.all(outs.map(load));
        const extra = (await Promise.allSettled(added.map(load))).flatMap((r) => (r.status === "fulfilled" ? [{ ...outputRun(r.value, keptIndex()), added: true }] : []));
        const own = restoredRun(t.id, loaded);
        const run: TabRun | null = own
          ? { ...own, kept: [...(own.kept ?? []), ...extra] }
          : extra.length
            ? { jobId: null, running: false, startedAt: Date.now(), finishedStatus: "success", statements: [], kept: extra, activeIndex: extra.at(-1)!.plan.index }
            : null;
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
        const keptGone = r.kept?.some((x) => x.output && gone.has(x.output.handle));
        if (!r.statements.some((x) => x.output && gone.has(x.output.handle)) && !keptGone) continue;
        // A dropped kept result leaves the tab; the shown one moves on if it was that.
        const kept = r.kept?.filter((x) => !(x.output && gone.has(x.output.handle)));
        const shownGone = r.activeIndex !== null && r.activeIndex < 0 && !kept?.some((x) => x.plan.index === r.activeIndex);
        // Only added results there, all gone: the tab is as before anything ran.
        if (!r.statements.length && !kept?.length && !r.running && r.kept?.every((x) => x.added)) {
          delete runs[k];
          continue;
        }
        runs[k] = {
          ...r,
          kept,
          activeIndex: shownGone ? (lastShown(r.statements) ?? kept?.at(-1)?.plan.index ?? null) : r.activeIndex,
          statements: r.statements.map((x) =>
            x.output && gone.has(x.output.handle) ? { ...x, output: undefined, result: undefined, notices: [...x.notices, `Output ${x.output.handle} was dropped. Run again to see rows.`] } : x,
          ),
        };
      }
      let tabs = s.tabs.filter((t) => !(t.output_ref && gone.has(t.output_ref)));
      // Never no tab at all (as when the last tab is closed), and the list as it is now is kept for the next start.
      if (tabs.length === 0) tabs = [{ id: uid(), title: "Query 1", sql: "", connection_id: s.tabs.find((t) => t.connection_id)?.connection_id ?? null }];
      if (tabs.length !== s.tabs.length) persistTabs(tabs);
      const activeTabId = tabs.some((t) => t.id === s.activeTabId) ? s.activeTabId : (tabs[0]?.id ?? null);
      // Added results whose output is gone are forgotten.
      let addedResults = s.addedResults;
      if (Object.values(addedResults).some((l) => l.some((a) => gone.has(a.handle)))) {
        addedResults = Object.fromEntries(Object.entries(addedResults).map(([k, l]) => [k, l.filter((a) => !gone.has(a.handle))]).filter(([, l]) => l.length));
        persistAdded(addedResults);
      }
      return { runs, tabs, activeTabId, addedResults, outputs: s.outputs.filter((o) => !gone.has(o.handle)) };
    }),
  applyOutput: (o) =>
    set((s) => {
      const runs = { ...s.runs };
      for (const [k, r] of Object.entries(runs)) {
        const mine = (x: StatementRun) => x.output?.handle === o.handle;
        if (r.statements.some(mine) || r.kept?.some(mine)) {
          const update = (x: StatementRun) => (mine(x) ? { ...x, output: o } : x);
          runs[k] = { ...r, statements: r.statements.map(update), kept: r.kept?.map(update) };
        }
      }
      return { runs, outputs: s.outputs.map((x) => (x.handle === o.handle ? o : x)) };
    }),
  runs: {},
  retireRunKeys: (keys) => {
    for (const k of keys) retiredKeys.add(k);
    get().forgetRuns(keys);
  },
  forgetRuns: (keys, prefix) => {
    const gone = Object.keys(get().runs).filter((k) => keys.includes(k) || (prefix !== undefined && k.startsWith(prefix)));
    if (!gone.length) return;
    set((s) => {
      const runs = { ...s.runs };
      for (const k of gone) delete runs[k];
      return { runs };
    });
  },
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
    if (mode === "all") {
      // Run all is the whole script, whatever is selected: a selected
      // `DELETE FROM t` must not run without the WHERE on its next line.
      sql = input.doc;
    } else if (input.selTo > input.selFrom) {
      sql = input.doc.slice(input.selFrom, input.selTo);
      base = input.selFrom;
    } else {
      let span: Awaited<ReturnType<typeof api.statementAtCursor>>;
      try {
        span = await api.statementAtCursor(conn.config.kind, input.doc, input.cursor);
      } catch (e) {
        s.toast(toError(e).message, "error");
        return;
      }
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
    // Closed while the statement or its parameters were looked up: nothing to run it for.
    if (!get().tabs.some((t) => t.id === tabId)) return;

    await get().runSql(tabId, conn.id, sql, base, undefined, undefined, values);
  },

  runSql: async (runKey, connectionId, sql, base = 0, sessionKey, outputName, params, rowLimit, replaces, origin) => {
    const conn = get().connections.find((c) => c.id === connectionId);
    // Closed while the run was starting (connecting, a confirmation, a
    // sign-in): what started is stopped and nothing is shown for it.
    const openAtStart = runKeyOpen(get(), runKey);
    const closed = () => retiredKeys.has(runKey) || (openAtStart && !runKeyOpen(get(), runKey));
    const submit = async (confirmed: boolean): Promise<boolean> => {
      if (closed()) return false;
      try {
        const req = {
          connection_id: connectionId,
          tab_id: runKey,
          sql,
          base_offset: base,
          row_limit: rowLimit !== undefined ? rowLimit : get().rowLimit > 0 ? get().rowLimit : null,
          confirmed,
          session_key: sessionKey ?? null,
          output_name: outputName ?? null,
          params: params ?? {},
          ...(origin ? { origin } : {}),
        };
        const resp = replaces ? await api.rerunOutput(req, replaces) : await api.runQuery(req);
        if (resp.status === "started" && closed()) {
          earlyEvents.delete(resp.job_id);
          void api.cancelQuery(resp.job_id).catch(() => false);
          // Its results too: the close went out before this run existed. The
          // shared session (a notebook's) only when what owns it closed: a
          // deleted cell's notebook goes on using it.
          void api.closeTab(runKey).catch(() => {});
          if (sessionKey && !runKeyOpen(get(), runKey)) void api.closeTab(sessionKey).catch(() => {});
          return false;
        }
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
        set((st) => {
          const prev = st.runs[runKey];
          const at = resp.statements[0]?.index;
          // Where that statement is in the run: by its index, not as a
          // position (a run restored after a restart lists only the
          // statements that had results).
          const pos = prev && at !== undefined ? prev.statements.findIndex((x) => x.plan.index === at) : -1;
          // One result refreshed: the run's other results stay as they were;
          // its statement keeps the place it has in the editor.
          if (replaces && prev && resp.statements.length === 1 && pos >= 0) {
            return {
              runs: {
                ...st.runs,
                [runKey]: {
                  ...prev,
                  jobId: resp.job_id,
                  running: true,
                  startedAt: Date.now(),
                  statements: prev.statements.map((x, i) => (i === pos ? { plan: x.plan, status: "pending", notices: [] } : x)),
                  activeIndex: at,
                  errorRange: undefined,
                  refreshOf: at,
                },
              },
            };
          }
          return {
            runs: {
              ...st.runs,
              [runKey]: {
                jobId: resp.job_id,
                running: true,
                startedAt: Date.now(),
                statements: resp.statements.map((p) => ({ plan: p, status: "pending", notices: [] })),
                kept: carriedResults(st.runs[runKey]),
                activeIndex: null,
                connectionId,
              },
            },
          };
        });
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
  refreshResult: async (runKey, index) => {
    const run = get().runs[runKey];
    const o = run?.statements[index]?.output;
    if (!run || run.running || !o) return false;
    // Its own SQL (parameters already in it), on its own session, under its name.
    return get().runSql(runKey, o.connection_id, o.sql, 0, o.session_key || undefined, o.name ?? null, {}, undefined, o.handle);
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

  keepResult: async (tabId, output, kept) => {
    try {
      const o = await api.keepOutput(output.handle, kept);
      get().applyOutput(o);
      if (!kept) {
        // A kept result of an earlier run leaves the tab now.
        set((s) => {
          const run = s.runs[tabId];
          const gone = run?.kept?.find((x) => x.output?.handle === o.handle);
          if (!run || !gone) return {};
          const shown = run.activeIndex === gone.plan.index ? lastShown(run.statements) : run.activeIndex;
          return { runs: { ...s.runs, [tabId]: { ...run, kept: run.kept?.filter((x) => x !== gone), activeIndex: shown } } };
        });
      }
    } catch (e) {
      get().toast(toError(e).message, "error");
    }
  },

  addedResults: {},
  addResult: async (tabId, output) => {
    const st = get();
    const run = st.runs[tabId];
    const shown = (x: StatementRun) => x.output?.handle === output.handle;
    if (output.tab_id === tabId || run?.statements.some(shown) || run?.kept?.some(shown)) {
      // Already here: shown instead.
      const at = run?.kept?.find(shown) ?? run?.statements.find(shown);
      if (at) st.setActiveStatement(tabId, at.plan.index);
      return;
    }
    // Not pinned (that would save every row in the workspace and keep it
    // for good): it is shown while its own tab keeps it.
    // As it is now: it may have been freed or dropped since the list was opened.
    const now = get().outputs.find((x) => x.handle === output.handle);
    if (!now || now.state === "evicted") {
      get().toast(`${outputLabel(output)} is no longer kept by its tab: run its query again to see it.`, "info");
      return;
    }
    let o = now;
    try {
      if (o.state === "on_disk") o = await api.loadOutput(o.handle);
    } catch (e) {
      get().toast(toError(e).message, "error");
      return;
    }
    const added: StatementRun = { ...outputRun(o, keptIndex()), added: true };
    set((s) => {
      // The tab closed meanwhile: nothing comes back for it.
      if (!s.tabs.some((t) => t.id === tabId)) return {};
      const r = s.runs[tabId];
      const next: TabRun = r
        ? { ...r, kept: [...(r.kept ?? []), added], activeIndex: added.plan.index }
        : { jobId: null, running: false, startedAt: Date.now(), finishedStatus: "success", statements: [], kept: [added], activeIndex: added.plan.index };
      const addedResults = { ...s.addedResults, [tabId]: [...(s.addedResults[tabId] ?? []), { handle: o.handle }] };
      persistAdded(addedResults);
      return { runs: { ...s.runs, [tabId]: next }, addedResults };
    });
  },
  removeAddedResult: (tabId, handle) => set((s) => withoutAdded(s, tabId, handle)),
  dropAddedResults: (tabId) =>
    set((s) => {
      if (!s.addedResults[tabId]) return {};
      const addedResults = { ...s.addedResults };
      delete addedResults[tabId];
      persistAdded(addedResults);
      return { addedResults };
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
      // An event names a statement by its index; the run lists statements by
      // position. The same in a run as it started; not in one restored after
      // a restart (only statements that had results), which a refresh updates.
      const posOf = (i: number) => (statements[i]?.plan.index === i ? i : statements.findIndex((x) => x.plan.index === i));
      const upd = (i: number, patch: Partial<StatementRun>) => {
        const p = posOf(i);
        if (statements[p]) statements[p] = { ...statements[p], ...patch };
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
              if (r?.jobId === job_id && r.statements.find((x) => x.plan.index === index)?.status === "running") void get().computeTips(tab_id, job_id);
            }, ms + 50);
          }
          break;
        }
        case "progress":
          upd(e.index, { progressRows: e.rows });
          break;
        case "downloading": {
          const cur = statements[posOf(e.index)];
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
            serverMs: e.server_ms ?? statements[posOf(e.index)]?.serverMs,
          });
          if (e.result) next.activeIndex = e.index;
          if (run.connectionId) {
            const id = run.connectionId;
            const plan = statements[posOf(e.index)]?.plan;
            const effect = plan ? ddlEffect(plan.sql, plan.classification.kind) : null;
            setTimeout(() => {
              get().markOnline(id);
              if (effect?.tables) get().schemaChanged(id, effect.schemaList);
            }, 0);
          }
          break;
        case "statement_failed": {
          const cancelled = e.error.kind === "cancelled";
          if (e.error.code === "oracle_client_missing") setTimeout(() => set({ oracleClientPrompt: true }), 0);
          announceSshHostKey(e.error);
          // After this update: the dialog's listener sets the store too.
          if (e.error.code === "password_expired") setTimeout(() => announcePasswordExpired(e.error, run.connectionId, true), 0);
          upd(e.index, {
            status: cancelled ? "cancelled" : "error",
            error: e.error,
            durationMs: e.duration_ms,
          });
          const st = statements[posOf(e.index)];
          if (!cancelled && st) next.errorRange = errorRange(st.plan, e.error.position);
          if (next.activeIndex === null || !cancelled) next.activeIndex = e.index;
          break;
        }
        case "job_finished":
          next = {
            ...next,
            running: false,
            // A refreshed result: the run's own status and time stand, unless the refresh failed.
            finishedStatus: run.refreshOf !== undefined && e.status === "success" ? run.finishedStatus : e.status,
            durationMs: run.refreshOf !== undefined ? run.durationMs : e.duration_ms,
            refreshOf: undefined,
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

/** State without result `handle` added to tab `tabId`: its tab leaves the run
 * (the shown one moves on), and the tab no longer remembers it. */
function withoutAdded(s: State, tabId: string, handle: string): Partial<State> {
  const r = s.runs[tabId];
  const runs = { ...s.runs };
  if (r) {
    const gone = r.kept?.find((x) => x.added && x.output?.handle === handle);
    const kept = r.kept?.filter((x) => x !== gone);
    const activeIndex = gone && r.activeIndex === gone.plan.index ? (lastShown(r.statements) ?? kept?.at(-1)?.plan.index ?? null) : r.activeIndex;
    // Nothing left to show: the tab is as before anything ran.
    if (!r.statements.length && !kept?.length) delete runs[tabId];
    else runs[tabId] = { ...r, kept, activeIndex };
  }
  const list = (s.addedResults[tabId] ?? []).filter((a) => a.handle !== handle);
  const addedResults = { ...s.addedResults };
  if (list.length) addedResults[tabId] = list;
  else delete addedResults[tabId];
  persistAdded(addedResults);
  return { runs, addedResults };
}

/** The results a new run of the tab keeps showing: those kept before, and
 * this run's the user kept (renumbered so they don't clash with the new run). */
function carriedResults(prev: TabRun | undefined): StatementRun[] | undefined {
  if (!prev) return undefined;
  const now = prev.statements.filter((s) => s.output?.kept && s.result).map((s) => ({ ...s, plan: { ...s.plan, index: keptIndex() } }));
  const all = [...(prev.kept ?? []), ...now];
  return all.length ? all : undefined;
}

/** `plan.index` of the last statement with something to show, else the
 * last one (as when a run finishes), else null. */
function lastShown(statements: StatementRun[]): number | null {
  const s = [...statements].reverse().find((x) => x.result || x.error) ?? statements.at(-1);
  return s ? s.plan.index : null;
}
