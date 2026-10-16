// AI mode state: providers, the current chat run, UI requests from the agent
// (approvals, editor proposals) and knowledge indexing progress.

import { create } from "zustand";
import { api, isTauri, onAiEvent, onKnowledgeEvent, toError } from "./lib/api";
import type {
  IndexPlan,
  AgentEvent,
  AiMode,
  AiUiRequest,
  EditProposal,
  PlannedStatement,
  ProviderUsage,
  ProviderView,
  UiContext,
} from "./lib/types";
import { uid } from "./lib/util";
import { useStore } from "./store";
import { editorBridge } from "./editorBridge";
import { referencedOutputs, withDuckdbHeader } from "./lib/dataflow";
import { resultsConnection } from "./outputs";

export type ChatItem =
  | { id: string; kind: "user"; text: string; mode: AiMode }
  | { id: string; kind: "assistant"; text: string; streaming: boolean }
  | {
      id: string;
      kind: "tool";
      callId: string;
      tool: string;
      args: Record<string, unknown>;
      content?: string;
      display?: unknown;
      done: boolean;
    }
  | {
      id: string;
      kind: "approval";
      requestId: string;
      tool: string;
      summary: string;
      detail: Record<string, unknown>;
      state: "pending" | "approved" | "denied";
    }
  | {
      id: string;
      kind: "edit";
      requestId: string;
      targetKey: string | null;
      proposal: EditProposal;
      before: string;
      state: "pending" | "accepted" | "rejected";
    }
  | {
      id: string;
      kind: "error";
      text: string;
      /** Sign-in expired or provider misconfigured: offer to open this provider's settings. */
      providerId?: string | null;
      /** The question that failed, to send again (after signing in again, or any error). */
      retry?: SendInput;
      /** The provider was signed in again since: offer the resend first. */
      reauthed?: boolean;
    };

export type AiView = "chat" | "history" | "knowledge";

export interface SendInput {
  message: string;
  mode?: AiMode;
  /** Editor key (tab id or notebook cell key). Defaults to the focused editor. */
  targetKey?: string | null;
  connectionId?: string | null;
  context?: UiContext;
}

interface AiState {
  open: boolean;
  view: AiView;
  providers: ProviderView[];
  providerId: string | null;
  model: string | null;
  sessionId: string | null;
  items: ChatItem[];
  runId: string | null;
  running: boolean;
  targetKey: string | null;
  connectionId: string | null;
  /** Other connections the assistant may use too (ids, kept across restarts). */
  alsoConnections: string[];
  setAlsoConnections: (ids: string[]) => void;
  tokens: { input: number; output: number };
  /** Kiro: credits this chat spent, and how full its context is (%). */
  credits: number;
  contextPercent: number | null;
  /** Budget and token totals per provider (loaded on demand). */
  usage: Record<string, { data?: ProviderUsage; error?: string; loading: boolean; at: number }>;
  /** Load a provider's usage; cached for a minute unless `force`. */
  loadUsage: (providerId: string, force?: boolean) => Promise<void>;
  knowledgeProgress: Record<string, { schema: string; done: number; total: number } | undefined>;
  knowledgeVersion: number;
  sessionsVersion: number;
  /** Scope picker shown before indexing a large connection. */
  indexPicker: { connectionId: string; plan: IndexPlan } | null;

  init: () => Promise<void>;
  setOpen: (open: boolean, view?: AiView) => void;
  setView: (view: AiView) => void;
  refreshProviders: () => Promise<void>;
  setProvider: (id: string | null, model?: string | null) => void;
  /** Models of each provider (shared by the AI panel and Settings, kept across restarts). */
  models: Record<string, string[]>;
  /** Load a provider's models (once; `force` asks again). Throws on failure. */
  loadModels: (providerId: string, force?: boolean) => Promise<string[]>;
  /** Provider to show when Settings opens (sign in again, fix its config). */
  settingsProvider: string | null;
  openProviderSettings: (providerId?: string | null) => void;
  send: (input: SendInput) => Promise<void>;
  cancel: () => Promise<void>;
  newChat: () => void;
  /** Questions asked while the assistant was working, sent in order when it finishes. */
  queued: (SendInput & { id: string })[];
  removeQueued: (id: string) => void;
  /** The question of the run in progress (kept for "Resend" when it fails). */
  lastInput: SendInput | null;
  /** Send a failed question again (the error card goes away). */
  resend: (errorItemId: string) => void;
  /** After signing in to a provider again: its errors offer "Resend question" first. */
  markReauthed: (providerId: string) => void;
  /** Send a waiting question now (after a failure or Stop, when it is not sent on its own). */
  sendQueued: (id: string) => void;
  loadSession: (id: string) => Promise<void>;
  respondApproval: (itemId: string, approved: boolean, editedSql?: string) => Promise<void>;
  respondEdit: (itemId: string, accepted: boolean) => Promise<void>;
  /**
   * Index a connection. Without `scope`, large connections whose scope was
   * never chosen open the scope picker instead; `choose` always opens it.
   */
  indexKnowledge: (connectionId: string, opts?: { scope?: string[]; batch?: number; choose?: boolean; full?: boolean }) => Promise<void>;
  cancelIndex: (connectionId: string) => Promise<void>;
  closeIndexPicker: () => void;
}

/** Connection for notebook cell editor keys (registered by the notebook view). */
const keyConnections = new Map<string, string | null>();
export function registerKeyConnection(key: string, connectionId: string | null) {
  keyConnections.set(key, connectionId);
}

/** Connection for an editor key: SQL tab's connection or the cell's. */
export function connectionForKey(key: string | null | undefined): string | null {
  if (!key) return null;
  if (keyConnections.has(key)) return keyConnections.get(key) ?? null;
  return useStore.getState().tabs.find((t) => t.id === key)?.connection_id ?? null;
}

/** Last result id and error message for a run key. */
export function runContext(key: string | null | undefined): {
  resultId: string | null;
  error: string | null;
  failed: PlannedStatement | null;
} {
  const run = key ? useStore.getState().runs[key] : undefined;
  if (!run) return { resultId: null, error: null, failed: null };
  const active = run.activeIndex !== null ? run.statements[run.activeIndex] : undefined;
  const failed = run.statements.find((s) => s.status === "error");
  return {
    resultId: active?.result?.id ?? run.statements.find((s) => s.result)?.result?.id ?? null,
    error: failed?.error?.message ?? null,
    failed: failed?.plan ?? null,
  };
}

/**
 * Where the failed statement is in the editor text now: at its run offsets
 * when unchanged, else the occurrence nearest to them. Null if it was edited away.
 */
export function locateStatement(doc: string, plan: Pick<PlannedStatement, "sql" | "start" | "end">): { from: number; to: number } | null {
  const sql = plan.sql.trim();
  if (!sql) return null;
  const at = doc.indexOf(sql, plan.start);
  if (at >= 0 && at <= plan.end) return { from: at, to: at + sql.length };
  let best = -1;
  for (let i = doc.indexOf(sql); i >= 0; i = doc.indexOf(sql, i + 1)) {
    if (best < 0 || Math.abs(i - plan.start) < Math.abs(best - plan.start)) best = i;
  }
  return best >= 0 ? { from: best, to: best + sql.length } : null;
}

/** 1-based editor line of `offset`. */
export function lineOf(doc: string, offset: number): number {
  let n = 1;
  for (let i = doc.indexOf("\n"); i >= 0 && i < offset; i = doc.indexOf("\n", i + 1)) n++;
  return n;
}

function defaultTargetKey(): string | null {
  const st = useStore.getState();
  const focused = editorBridge.focused();
  const active = st.tabs.find((t) => t.id === st.activeTabId);
  if (focused && (focused === active?.id || (active?.notebook_id && focused.startsWith(`nb:${active.notebook_id}:`)))) {
    return focused;
  }
  // Only query tabs have an editor to read or write.
  return active?.notebook_id || active?.ddl || active?.job_id || active?.lineage || active?.output_ref ? null : (active?.id ?? null);
}

/** What the assistant is doing now, for the status line while it works. */
export function workingStatus(items: ChatItem[]): { text: string; waitingForYou: boolean } {
  const last = items[items.length - 1];
  if (items.some((i) => i.kind === "approval" && i.state === "pending")) return { text: "Waiting for your approval above", waitingForYou: true };
  if (items.some((i) => i.kind === "edit" && i.state === "pending")) return { text: "Waiting for you to accept or reject the change above", waitingForYou: true };
  const tool = [...items].reverse().find((i) => i.kind === "tool" && !i.done);
  if (tool && tool.kind === "tool") return { text: `Working: ${tool.tool.replace(/_/g, " ")}…`, waitingForYou: false };
  if (last?.kind === "assistant" && last.streaming) return { text: "Writing the answer…", waitingForYou: false };
  return { text: "Thinking…", waitingForYou: false };
}

let aiInitStarted = false;

/** An error the user fixes in the provider's settings (expired sign-in, bad key, missing model). */
export function needsProviderSettings(e: { kind: string; message: string }): boolean {
  if (e.kind === "auth") return true;
  if (e.kind === "policy") return false;
  return /not signed in|sign in|signed out|log ?in|expired|unauthori[sz]ed|forbidden|invalid (api )?key|\b40[13]\b|choose a model|add an ai provider/i.test(e.message);
}

const MODELS_KEY = "db.ai.models";
function savedModels(): Record<string, string[]> {
  try {
    const v = JSON.parse(localStorage.getItem(MODELS_KEY) ?? "{}");
    return v && typeof v === "object" ? v : {};
  } catch {
    return {};
  }
}
const modelLoads = new Map<string, Promise<string[]>>();

/**
 * Items to show. A tool call waiting for approval is shown once, as the
 * approval card: its own "running" card (same SQL) is hidden until it ends.
 */
export function visibleItems(items: ChatItem[]): ChatItem[] {
  return items.filter((it, i) => {
    if (it.kind !== "tool" || it.done) return true;
    return !items.slice(i + 1).some((n) => n.kind === "approval" && n.tool === it.tool && n.state === "pending");
  });
}

export const useAi = create<AiState>((set, get) => {
  /** Events and UI requests that arrived before ai_send returned the run id. */
  let early: AgentEvent[] = [];
  let earlyRequests: AiUiRequest[] = [];
  const patchItem = (id: string, patch: Partial<ChatItem>) =>
    set((s) => ({ items: s.items.map((i) => (i.id === id ? ({ ...i, ...patch } as ChatItem) : i)) }));

  const handleAgent = (e: AgentEvent) => {
    if (e.run_id !== get().runId) {
      // Events of the run being started arrive before ai_send returns its id:
      // keep them and replay them once the id is known (dropping "started"
      // lost the session id, so every question began a new conversation).
      if (get().running && get().runId === null) early.push(e);
      return;
    }
    // The conversation continues in this session, whichever event says so first.
    if (e.session_id && get().sessionId !== e.session_id) set({ sessionId: e.session_id });
    switch (e.type) {
      case "started":
        break;
      case "text_delta":
        set((s) => {
          const last = s.items[s.items.length - 1];
          if (last?.kind === "assistant" && last.streaming) {
            return { items: [...s.items.slice(0, -1), { ...last, text: last.text + e.text }] };
          }
          return { items: [...s.items, { id: uid(), kind: "assistant", text: e.text, streaming: true }] };
        });
        break;
      case "tool_started":
        if (get().items.some((i) => i.kind === "tool" && i.callId === e.call_id)) break;
        set((s) => ({
          items: [
            ...s.items.map((i) => (i.kind === "assistant" ? { ...i, streaming: false } : i)),
            { id: uid(), kind: "tool", callId: e.call_id, tool: e.tool, args: e.args ?? {}, done: false },
          ],
        }));
        break;
      case "tool_finished":
        set((s) => ({
          items: s.items.map((i) =>
            i.kind === "tool" && i.callId === e.call_id ? { ...i, done: true, content: e.content, display: e.display } : i,
          ),
        }));
        if (e.tool === "add_knowledge_note" || e.tool === "update_knowledge_note") set((s) => ({ knowledgeVersion: s.knowledgeVersion + 1 }));
        if (e.tool === "save_query") void useStore.getState().refreshSavedQueries();
        if (e.tool === "run_job") void import("./jobsStore").then(({ useJobs }) => useJobs.getState().refresh().catch(() => {}));
        if (e.tool === "create_job" || e.tool === "update_job") {
          // Show the new job: refresh the Jobs list and open it in a tab.
          const d = e.display as { job_id?: string; name?: string } | null;
          if (d?.job_id) {
            const { job_id, name } = d;
            // An open job tab shows the new version.
            if (e.tool === "update_job") window.dispatchEvent(new CustomEvent("db:job-changed", { detail: { id: job_id } }));
            void import("./jobsStore").then(({ useJobs }) =>
              useJobs
                .getState()
                .refresh()
                .then(() => useJobs.getState().open(job_id, name ?? "Job"))
                .catch(() => {}),
            );
          }
        }
        break;
      case "usage":
        set((s) => ({ tokens: { input: s.tokens.input + e.input, output: s.tokens.output + e.output } }));
        break;
      case "metering":
        set((s) => ({ credits: s.credits + e.credits, contextPercent: e.context_percent ?? s.contextPercent }));
        break;
      case "finished":
      case "failed":
        // The budget moved: refresh it if it is shown.
        if (get().providerId && get().usage[get().providerId!]) void get().loadUsage(get().providerId!, true);
        set((s) => ({
          running: false,
          runId: null,
          sessionsVersion: s.sessionsVersion + 1,
          items: [
            ...s.items.map((i): ChatItem => {
              if (i.kind === "assistant") return { ...i, streaming: false };
              if (i.kind === "tool" && !i.done) return { ...i, done: true };
              if (i.kind === "approval" && i.state === "pending") return { ...i, state: "denied" };
              if (i.kind === "edit" && i.state === "pending") return { ...i, state: "rejected" };
              return i;
            }),
            ...(e.type === "failed" && e.error.kind !== "cancelled"
              ? [
                  {
                    id: uid(),
                    kind: "error" as const,
                    text: e.error.message,
                    providerId: needsProviderSettings(e.error) ? s.providerId : undefined,
                    retry: s.lastInput ?? undefined,
                  },
                ]
              : []),
          ],
        }));
        // The next question asked meanwhile goes out now. After a failure or
        // Stop it waits for the user (Send now / remove).
        if (e.type === "finished") {
          const [next, ...rest] = get().queued;
          if (next) {
            set({ queued: rest });
            setTimeout(() => void get().send(next), 0);
          }
        }
        break;
    }
  };

  const handleRequest = (r: AiUiRequest) => {
    if (r.run_id !== get().runId) {
      if (get().running && get().runId === null) {
        earlyRequests.push(r);
        return;
      }
      // Stale run: decline so the backend does not wait for the timeout.
      void api.aiRespond(r.request_id, r.type === "edit_proposal" ? { accepted: false } : { approved: false });
      return;
    }
    const key = get().targetKey;
    if (r.type === "editor_request") {
      const snap = editorBridge.snapshot(key);
      const ctx = runContext(key);
      void api.aiRespond(
        r.request_id,
        snap
          ? { sql: snap.sql, selection: snap.selection, last_error: ctx.error, failed_sql: ctx.failed?.sql.trim() ?? null, result_id: ctx.resultId }
          : { sql: null, note: "No editor is open" },
      );
      return;
    }
    if (r.type === "approval_request") {
      if (get().items.some((i) => i.kind === "approval" && i.requestId === r.request_id)) return;
      set((s) => ({
        items: [
          ...s.items,
          { id: uid(), kind: "approval", requestId: r.request_id, tool: r.tool, summary: r.summary, detail: r.detail ?? {}, state: "pending" },
        ],
      }));
      return;
    }
    if (get().items.some((i) => i.kind === "edit" && i.requestId === r.request_id)) return;
    const target = r.proposal.mode === "new_tab" ? null : key;
    set((s) => ({
      items: [
        ...s.items,
        {
          id: uid(),
          kind: "edit",
          requestId: r.request_id,
          targetKey: target,
          proposal: r.proposal,
          before: target ? (editorBridge.snapshot(target)?.selection ?? editorBridge.snapshot(target)?.sql ?? "") : "",
          state: "pending",
        },
      ],
    }));
  };

  return {
    open: localStorage.getItem("db.ai.open") === "1",
    view: "chat",
    providers: [],
    providerId: null,
    model: null,
    sessionId: null,
    items: [],
    runId: null,
    running: false,
    targetKey: null,
    connectionId: null,
    alsoConnections: (() => {
      try {
        const v = JSON.parse(localStorage.getItem("db.ai.alsoConns") ?? "[]");
        return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
      } catch {
        return [];
      }
    })(),
    setAlsoConnections: (ids) => {
      const uniq = [...new Set(ids)];
      localStorage.setItem("db.ai.alsoConns", JSON.stringify(uniq));
      set({ alsoConnections: uniq });
    },
    tokens: { input: 0, output: 0 },
    credits: 0,
    contextPercent: null,
    usage: {},
    loadUsage: async (providerId, force = false) => {
      const cur = get().usage[providerId];
      if (cur?.loading || (!force && cur?.data && Date.now() - cur.at < 60_000)) return;
      set((s) => ({ usage: { ...s.usage, [providerId]: { ...cur, loading: true, at: cur?.at ?? 0 } } }));
      const now = new Date();
      const day = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
      const month = new Date(now.getFullYear(), now.getMonth(), 1).getTime();
      try {
        const data = await api.aiProviderUsage(providerId, day, month);
        set((s) => ({ usage: { ...s.usage, [providerId]: { data, loading: false, at: Date.now() } } }));
      } catch (e) {
        set((s) => ({ usage: { ...s.usage, [providerId]: { ...s.usage[providerId], error: toError(e).message, loading: false, at: Date.now() } } }));
      }
    },
    knowledgeProgress: {},
    knowledgeVersion: 0,
    indexPicker: null,
    sessionsVersion: 0,
    models: savedModels(),
    queued: [],
    lastInput: null,
    resend: (itemId) => {
      const it = get().items.find((i) => i.id === itemId);
      if (it?.kind !== "error" || !it.retry) return;
      const retry = it.retry;
      set((s) => ({ items: s.items.filter((i) => i.id !== itemId) }));
      void get().send(retry);
    },
    markReauthed: (providerId) =>
      set((s) => ({
        items: s.items.map((i) => (i.kind === "error" && i.retry && (i.providerId === providerId || i.providerId === null) ? { ...i, reauthed: true } : i)),
      })),
    settingsProvider: null,

    loadModels: (providerId, force = false) => {
      const have = get().models[providerId];
      if (have?.length && !force) return Promise.resolve(have);
      const running = modelLoads.get(providerId);
      if (running) return running;
      const p = api
        .aiListModels(providerId)
        .then((list) => {
          const ids = [...new Set(list.map((m) => m.id))];
          const models = { ...get().models, [providerId]: ids };
          set({ models });
          try {
            localStorage.setItem(MODELS_KEY, JSON.stringify(models));
          } catch {
            /* storage full: kept in memory */
          }
          return ids;
        })
        .finally(() => modelLoads.delete(providerId));
      modelLoads.set(providerId, p);
      return p;
    },

    openProviderSettings: (providerId) => {
      set({ settingsProvider: providerId ?? get().providerId });
      useStore.getState().setSettingsOpen(true);
    },

    init: async () => {
      // Once: a second listener (StrictMode double effect) showed every
      // approval and tool card twice.
      if (!isTauri() || aiInitStarted) return;
      aiInitStarted = true;
      await onAiEvent((e) => {
        if (e.type === "approval_request" || e.type === "edit_proposal" || e.type === "editor_request") handleRequest(e);
        else handleAgent(e);
      });
      await onKnowledgeEvent((e) => {
        if (e.type === "progress") {
          set((s) => ({ knowledgeProgress: { ...s.knowledgeProgress, [e.connection_id]: { schema: e.schema, done: e.done, total: e.total } } }));
          return;
        }
        set((s) => ({
          knowledgeProgress: { ...s.knowledgeProgress, [e.connection_id]: undefined },
          knowledgeVersion: s.knowledgeVersion + 1,
        }));
        const toast = useStore.getState().toast;
        if (e.type === "failed") toast(`Indexing failed: ${e.error}`, "error");
        else if (e.type === "cancelled") toast("Indexing cancelled", "info");
        else if (e.report.cancelled)
          toast(`Indexing cancelled. Kept ${e.report.objects} objects already indexed.`, "info");
        else
          toast(
            `Indexed ${e.report.objects} objects in ${e.report.schemas} schemas` +
              (e.report.skipped ? ` (${e.report.skipped} unchanged, not re-read)` : "") +
              (e.report.errors.length ? ` (${e.report.errors.length} errors)` : ""),
            e.report.errors.length ? "info" : "success",
          );
      });
      const [settings] = await Promise.all([api.getSettings(), get().refreshProviders()]);
      const pid = typeof settings.ai_provider === "string" ? settings.ai_provider : null;
      const providers = get().providers;
      const provider = providers.find((p) => p.id === pid) ?? providers[0];
      set({
        providerId: provider?.id ?? null,
        model: typeof settings.ai_model === "string" && provider?.id === pid ? settings.ai_model : (provider?.config.default_model ?? null),
      });
    },

    setOpen: (open, view) => {
      localStorage.setItem("db.ai.open", open ? "1" : "0");
      set(view ? { open, view } : { open });
    },
    setView: (view) => set({ view }),

    refreshProviders: async () => {
      const providers = await api.aiListProviders();
      set((s) => ({
        providers,
        providerId: providers.some((p) => p.id === s.providerId) ? s.providerId : (providers[0]?.id ?? null),
      }));
    },

    setProvider: (id, model) => {
      const p = get().providers.find((x) => x.id === id);
      const m = model !== undefined ? model : (p?.config.default_model ?? null);
      set({ providerId: id, model: m });
      void api.setSetting("ai_provider", id).catch(() => {});
      void api.setSetting("ai_model", m).catch(() => {});
    },

    send: async (input) => {
      const { message, mode = "chat", targetKey, connectionId, context } = input;
      const st = useStore.getState();
      if (!st.backendAvailable) return st.toast("AI needs the desktop app", "error");
      if (get().running) {
        // Asked while the assistant works: sent when the current answer is done.
        set((s) => ({ queued: [...s.queued, { ...input, id: uid() }] }));
        return;
      }
      if (!get().providerId) {
        st.toast("Add an AI provider first", "info");
        st.setSettingsOpen(true);
        return;
      }
      const key = targetKey !== undefined ? targetKey : defaultTargetKey();
      const conn = connectionId ?? connectionForKey(key) ?? get().connectionId;
      if (!conn) return st.toast("Choose a connection for the assistant", "error");
      const rc = runContext(key);
      // Fix error: the statement that failed is the one to fix, wherever the
      // cursor is. Select it so the fix replaces only that statement.
      let failedSql: string | null = null;
      let failedLine: number | null = null;
      if (mode === "fix_error" && rc.failed) {
        const doc = editorBridge.snapshot(key)?.sql;
        const at = doc !== undefined ? locateStatement(doc, rc.failed) : null;
        if (doc !== undefined && at) {
          editorBridge.select(key, at.from, at.to);
          failedLine = lineOf(doc, at.from);
        }
        failedSql = rc.failed.sql.trim();
      }
      const snap = editorBridge.snapshot(key);
      const ctx: UiContext = {
        editor_sql: snap?.sql ?? null,
        selection: snap?.selection ?? null,
        last_error: rc.error,
        failed_sql: failedSql,
        failed_line: failedLine,
        result_id: rc.resultId,
        // Connections chosen in the panel besides this one (AI on, still saved).
        connections: get().alsoConnections.filter((id) => id !== conn && useStore.getState().connections.some((c) => c.id === id && c.ai_policy?.ai_enabled !== false)),
        ...context,
      };
      // One conversation across questions, also after switching connections
      // (each turn is told which connection it is on). New chat starts over.
      const sessionId = get().sessionId;
      set((s) => ({
        open: true,
        view: "chat",
        items: sessionId ? [...s.items, { id: uid(), kind: "user", text: message, mode }] : [{ id: uid(), kind: "user", text: message, mode }],
        sessionId,
        running: true,
        runId: null,
        lastInput: input,
        targetKey: key,
        connectionId: conn,
        tokens: sessionId ? s.tokens : { input: 0, output: 0 },
        credits: sessionId ? s.credits : 0,
        contextPercent: sessionId ? s.contextPercent : null,
      }));
      localStorage.setItem("db.ai.open", "1");
      try {
        const runId = await api.aiSend({
          session_id: sessionId,
          connection_id: conn,
          provider_id: get().providerId,
          model: get().model,
          message,
          mode,
          context: ctx,
          tab_id: key,
        });
        set({ runId });
        // Replay what this run sent before its id was known.
        const evs = early.filter((x) => x.run_id === runId);
        const reqs = earlyRequests.filter((x) => x.run_id === runId);
        early = [];
        earlyRequests = [];
        for (const x of evs) handleAgent(x);
        for (const x of reqs) handleRequest(x);
      } catch (e) {
        const err = toError(e);
        set((s) => ({
          running: false,
          items: [...s.items, { id: uid(), kind: "error", text: err.message, providerId: needsProviderSettings(err) ? s.providerId : undefined, retry: input }],
        }));
      }
    },

    cancel: async () => {
      const id = get().runId;
      if (id) await api.aiCancel(id);
    },

    newChat: () => {
      if (get().running) void get().cancel();
      set({ sessionId: null, items: [], runId: null, running: false, queued: [], tokens: { input: 0, output: 0 }, credits: 0, contextPercent: null, view: "chat" });
    },

    removeQueued: (id) => set((s) => ({ queued: s.queued.filter((q) => q.id !== id) })),
    sendQueued: (id) => {
      const q = get().queued.find((x) => x.id === id);
      if (!q || get().running) return;
      set((s) => ({ queued: s.queued.filter((x) => x.id !== id) }));
      void get().send(q);
    },

    loadSession: async (id) => {
      try {
        const [msgs, sessions] = await Promise.all([api.aiSessionMessages(id), api.aiSessions(null)]);
        const sess = sessions.find((s) => s.id === id);
        const items: ChatItem[] = [];
        for (const m of msgs) {
          const c = typeof m.content === "string" ? { text: m.content } : m.content;
          if (m.role === "user" && c.text) items.push({ id: uid(), kind: "user", text: c.text, mode: "chat" });
          else if (m.role === "assistant") {
            if (c.text) items.push({ id: uid(), kind: "assistant", text: c.text, streaming: false });
            for (const tc of c.tool_calls ?? [])
              items.push({ id: uid(), kind: "tool", callId: tc.id, tool: tc.name, args: (tc.arguments as Record<string, unknown>) ?? {}, done: true });
          } else if (m.role === "tool") {
            const last = [...items].reverse().find((i) => i.kind === "tool" && !i.content);
            if (last && last.kind === "tool") last.content = c.text ?? "";
          }
        }
        // Tokens the stored conversation used (Kiro credits are not stored).
        const tokens = msgs.reduce((t, m) => ({ input: t.input + (m.tokens_in ?? 0), output: t.output + (m.tokens_out ?? 0) }), { input: 0, output: 0 });
        set({ sessionId: id, items, connectionId: sess?.connection_id ?? get().connectionId, view: "chat", runId: null, running: false, tokens, credits: 0, contextPercent: null });
      } catch (e) {
        useStore.getState().toast(toError(e).message, "error");
      }
    },

    respondApproval: async (itemId, approved, editedSql) => {
      const item = get().items.find((i) => i.id === itemId);
      if (!item || item.kind !== "approval" || item.state !== "pending") return;
      const detail = editedSql !== undefined ? { ...item.detail, sql: editedSql } : item.detail;
      patchItem(itemId, { state: approved ? "approved" : "denied", detail } as Partial<ChatItem>);
      await api.aiRespond(item.requestId, { approved, detail });
    },

    respondEdit: async (itemId, accepted) => {
      const item = get().items.find((i) => i.id === itemId);
      if (!item || item.kind !== "edit" || item.state !== "pending") return;
      let ok = accepted;
      if (accepted) {
        // SQL over outputs (results.<name>) runs on the Results (DuckDB) connection.
        const readsOutputs = referencedOutputs(item.proposal.sql).length > 0;
        const st = useStore.getState();
        const tab = item.targetKey ? st.tabs.find((t) => t.id === item.targetKey) : undefined;
        const tabKind = st.connections.find((c) => c.id === tab?.connection_id)?.config.kind;
        if (item.proposal.mode === "new_tab" || !item.targetKey) {
          if (readsOutputs) {
            const results = await resultsConnection().catch(() => null);
            st.newTab({ title: item.proposal.title || "AI query", sql: withDuckdbHeader(item.proposal.sql), connection_id: results ?? get().connectionId });
          } else {
            st.newTab({ title: item.proposal.title || "AI query", sql: item.proposal.sql, connection_id: get().connectionId });
          }
        } else if (readsOutputs && tab && tabKind !== "duckdb" && item.proposal.mode === "replace_all") {
          // The whole query tab becomes a query over outputs: switch it to Results.
          const results = await resultsConnection().catch(() => null);
          const sql = withDuckdbHeader(item.proposal.sql);
          if (!editorBridge.apply(item.targetKey, { ...item.proposal, sql })) {
            st.toast("The target editor was closed", "error");
            ok = false;
          } else if (results) {
            st.updateTab(tab.id, { connection_id: results });
            st.toast("This query reads outputs, so the tab now runs on Results (DuckDB)", "info");
          }
        } else if (!editorBridge.apply(item.targetKey, item.proposal, item.before)) {
          useStore.getState().toast("The target editor was closed", "error");
          ok = false;
        } else if (readsOutputs && tab && tabKind !== "duckdb") {
          st.toast("The inserted SQL reads outputs (results.<name>); run it on the Results (DuckDB) connection", "info");
        }
      }
      patchItem(itemId, { state: ok ? "accepted" : "rejected" } as Partial<ChatItem>);
      await api.aiRespond(item.requestId, { accepted: ok });
    },

    indexKnowledge: async (connectionId, opts = {}) => {
      const toast = useStore.getState().toast;
      if (!opts.scope) {
        // Size the run first (one schema listing) and ask when it is large.
        set((s) => ({ knowledgeProgress: { ...s.knowledgeProgress, [connectionId]: { schema: "Checking size…", done: 0, total: 0 } } }));
        let plan: IndexPlan;
        try {
          plan = await api.knPlan(connectionId);
        } catch (e) {
          set((s) => ({ knowledgeProgress: { ...s.knowledgeProgress, [connectionId]: undefined } }));
          toast(toError(e).message, "error");
          return;
        }
        set((s) => ({ knowledgeProgress: { ...s.knowledgeProgress, [connectionId]: undefined } }));
        if (opts.choose || (plan.large && plan.scope.length === 0)) {
          set({ indexPicker: { connectionId, plan } });
          return;
        }
      }
      set((s) => ({ indexPicker: null, knowledgeProgress: { ...s.knowledgeProgress, [connectionId]: { schema: "", done: 0, total: 0 } } }));
      try {
        await api.knIndex(connectionId, opts.scope ?? null, opts.batch ?? null, opts.full ?? false);
        // The chosen scope / batch size are saved on the connection.
        if (opts.scope || opts.batch) void useStore.getState().refreshConnections();
      } catch (e) {
        set((s) => ({ knowledgeProgress: { ...s.knowledgeProgress, [connectionId]: undefined } }));
        toast(toError(e).message, "error");
      }
    },
    cancelIndex: async (connectionId) => {
      try {
        const running = await api.knCancel(connectionId);
        if (!running) set((s) => ({ knowledgeProgress: { ...s.knowledgeProgress, [connectionId]: undefined } }));
        else set((s) => ({ knowledgeProgress: { ...s.knowledgeProgress, [connectionId]: { ...(s.knowledgeProgress[connectionId] ?? { done: 0, total: 0 }), schema: "Cancelling…" } } }));
      } catch (e) {
        useStore.getState().toast(toError(e).message, "error");
      }
    },
    closeIndexPicker: () => set({ indexPicker: null }),
  };
});
