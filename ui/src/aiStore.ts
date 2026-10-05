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
  ProviderView,
  UiContext,
} from "./lib/types";
import { uid } from "./lib/util";
import { useStore } from "./store";
import { editorBridge } from "./editorBridge";

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
  | { id: string; kind: "error"; text: string };

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
  tokens: { input: number; output: number };
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
  send: (input: SendInput) => Promise<void>;
  cancel: () => Promise<void>;
  newChat: () => void;
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
export function runContext(key: string | null | undefined): { resultId: string | null; error: string | null } {
  const run = key ? useStore.getState().runs[key] : undefined;
  if (!run) return { resultId: null, error: null };
  const active = run.activeIndex !== null ? run.statements[run.activeIndex] : undefined;
  const failed = run.statements.find((s) => s.status === "error");
  return {
    resultId: active?.result?.id ?? run.statements.find((s) => s.result)?.result?.id ?? null,
    error: failed?.error?.message ?? null,
  };
}

function defaultTargetKey(): string | null {
  const st = useStore.getState();
  const focused = editorBridge.focused();
  const active = st.tabs.find((t) => t.id === st.activeTabId);
  if (focused && (focused === active?.id || (active?.notebook_id && focused.startsWith(`nb:${active.notebook_id}:`)))) {
    return focused;
  }
  return active?.notebook_id ? null : (active?.id ?? null);
}

let aiInitStarted = false;

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
  const patchItem = (id: string, patch: Partial<ChatItem>) =>
    set((s) => ({ items: s.items.map((i) => (i.id === id ? ({ ...i, ...patch } as ChatItem) : i)) }));

  const handleAgent = (e: AgentEvent) => {
    if (e.run_id !== get().runId) return;
    switch (e.type) {
      case "started":
        set({ sessionId: e.session_id });
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
        break;
      case "usage":
        set((s) => ({ tokens: { input: s.tokens.input + e.input, output: s.tokens.output + e.output } }));
        break;
      case "finished":
      case "failed":
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
              ? [{ id: uid(), kind: "error" as const, text: e.error.message }]
              : []),
          ],
        }));
        break;
    }
  };

  const handleRequest = (r: AiUiRequest) => {
    if (r.run_id !== get().runId) {
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
        snap ? { sql: snap.sql, selection: snap.selection, last_error: ctx.error, result_id: ctx.resultId } : { sql: null, note: "No editor is open" },
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
    tokens: { input: 0, output: 0 },
    knowledgeProgress: {},
    knowledgeVersion: 0,
    indexPicker: null,
    sessionsVersion: 0,

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

    send: async ({ message, mode = "chat", targetKey, connectionId, context }) => {
      const st = useStore.getState();
      if (!st.backendAvailable) return st.toast("AI needs the desktop app", "error");
      if (get().running) return st.toast("The assistant is still working — stop it first", "info");
      if (!get().providerId) {
        st.toast("Add an AI provider first", "info");
        st.setSettingsOpen(true);
        return;
      }
      const key = targetKey !== undefined ? targetKey : defaultTargetKey();
      const conn = connectionId ?? connectionForKey(key) ?? get().connectionId;
      if (!conn) return st.toast("Choose a connection for the assistant", "error");
      const snap = editorBridge.snapshot(key);
      const rc = runContext(key);
      const ctx: UiContext = {
        editor_sql: snap?.sql ?? null,
        selection: snap?.selection ?? null,
        last_error: rc.error,
        result_id: rc.resultId,
        ...context,
      };
      // A different connection starts a new session.
      const sessionId = get().connectionId === conn ? get().sessionId : null;
      set((s) => ({
        open: true,
        view: "chat",
        items: sessionId ? [...s.items, { id: uid(), kind: "user", text: message, mode }] : [{ id: uid(), kind: "user", text: message, mode }],
        sessionId,
        running: true,
        targetKey: key,
        connectionId: conn,
        tokens: sessionId ? s.tokens : { input: 0, output: 0 },
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
      } catch (e) {
        set((s) => ({ running: false, items: [...s.items, { id: uid(), kind: "error", text: toError(e).message }] }));
      }
    },

    cancel: async () => {
      const id = get().runId;
      if (id) await api.aiCancel(id);
    },

    newChat: () => {
      if (get().running) void get().cancel();
      set({ sessionId: null, items: [], runId: null, running: false, tokens: { input: 0, output: 0 }, view: "chat" });
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
        set({ sessionId: id, items, connectionId: sess?.connection_id ?? get().connectionId, view: "chat", runId: null, running: false });
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
        if (item.proposal.mode === "new_tab" || !item.targetKey) {
          useStore.getState().newTab({
            title: item.proposal.title || "AI query",
            sql: item.proposal.sql,
            connection_id: get().connectionId,
          });
        } else if (!editorBridge.apply(item.targetKey, item.proposal)) {
          useStore.getState().toast("The target editor was closed", "error");
          ok = false;
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
