// AI mode state: providers, the open conversations (each with its own run,
// queue and approvals; several can work at once), UI requests from the agent
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
      /** Approved by "Auto-approve" rather than by hand. */
      auto?: boolean;
    }
  | {
      id: string;
      kind: "edit";
      requestId: string;
      targetKey: string | null;
      proposal: EditProposal;
      before: string;
      state: "pending" | "accepted" | "rejected";
      /** Applied by "Auto-approve" rather than by hand. */
      auto?: boolean;
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
  /** Conversation to ask in (default: the one shown). */
  chatId?: string;
}

/** One conversation (a tab in the AI panel). Several can work at the same time. */
export interface Conversation {
  id: string;
  sessionId: string | null;
  /** Saved title (from history), else the first question is used. */
  title: string | null;
  items: ChatItem[];
  runId: string | null;
  running: boolean;
  targetKey: string | null;
  connectionId: string | null;
  tokens: { input: number; output: number };
  credits: number;
  contextPercent: number | null;
  queued: (SendInput & { id: string })[];
  lastInput: SendInput | null;
  autoApprove: boolean;
  /** Finished while another conversation was shown. */
  unread: boolean;
  /** Its saved messages are being read. */
  loading: boolean;
  /** Restored tab whose messages are read when it is shown. */
  needsLoad?: boolean;
}

/** Fields of the shown conversation mirrored at the top of the store. */
type Mirrored = Pick<
  Conversation,
  "sessionId" | "items" | "runId" | "running" | "targetKey" | "connectionId" | "tokens" | "credits" | "contextPercent" | "queued" | "lastInput" | "autoApprove"
>;

function mirror(c: Conversation): Mirrored {
  return {
    sessionId: c.sessionId,
    items: c.items,
    runId: c.runId,
    running: c.running,
    targetKey: c.targetKey,
    connectionId: c.connectionId,
    tokens: c.tokens,
    credits: c.credits,
    contextPercent: c.contextPercent,
    queued: c.queued,
    lastInput: c.lastInput,
    autoApprove: c.autoApprove,
  };
}

export function emptyConversation(connectionId: string | null = null): Conversation {
  return {
    id: uid(),
    sessionId: null,
    title: null,
    items: [],
    runId: null,
    running: false,
    targetKey: null,
    connectionId,
    tokens: { input: 0, output: 0 },
    credits: 0,
    contextPercent: null,
    queued: [],
    lastInput: null,
    autoApprove: false,
    unread: false,
    loading: false,
  };
}

/** Tab label: the saved title, else the first question. */
export function conversationTitle(c: Conversation): string {
  if (c.title) return c.title;
  const q = c.items.find((i) => i.kind === "user");
  return q && q.kind === "user" ? q.text.replace(/\s+/g, " ").slice(0, 60) : "New chat";
}

function isBlank(c: Conversation): boolean {
  return !c.sessionId && !c.running && !c.loading && c.items.length === 0 && c.queued.length === 0;
}

const CHATS_KEY = "db.ai.chats";
/** Most conversations open at once (the oldest idle one closes for a new one). */
export const MAX_CHATS = 12;

interface AiState extends Mirrored {
  open: boolean;
  view: AiView;
  /** Open conversations, in tab order. The fields below mirror the shown one. */
  chats: Conversation[];
  activeChatId: string;
  /** Start a conversation (reuses a blank shown one). Returns its id. */
  newChat: (opts?: { activate?: boolean }) => string;
  switchChat: (id: string) => void;
  /** Close a conversation's tab (stops it if it is working; it stays in History). */
  closeChat: (id: string) => void;
  /** History entry deleted: close its tab. */
  forgetSession: (sessionId: string) => void;
  providers: ProviderView[];
  providerId: string | null;
  model: string | null;
  /** Other connections the assistant may use too (ids, kept across restarts). */
  alsoConnections: string[];
  setAlsoConnections: (ids: string[]) => void;
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
  /** Stop a conversation's run (default: the shown one). */
  cancel: (chatId?: string) => Promise<void>;
  removeQueued: (id: string) => void;
  /** Send a failed question again (the error card goes away). */
  resend: (errorItemId: string) => void;
  /** After signing in to a provider again: its errors offer "Resend question" first. */
  markReauthed: (providerId: string) => void;
  /** Send a waiting question now (after a failure or Stop, when it is not sent on its own). */
  sendQueued: (id: string) => void;
  loadSession: (id: string) => Promise<void>;
  respondApproval: (itemId: string, approved: boolean, editedSql?: string, auto?: boolean) => Promise<void>;
  respondEdit: (itemId: string, accepted: boolean, auto?: boolean) => Promise<void>;
  /** Approve every request of the shown conversation (queries, writes, jobs, edits) without asking. Off for a new or another conversation. */
  setAutoApprove: (on: boolean) => void;
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

  const chat = (id: string | null | undefined) => (id ? get().chats.find((c) => c.id === id) : undefined);
  const byRun = (runId: string) => get().chats.find((c) => c.runId === runId);
  /** A conversation waits for ai_send to return its run id. */
  const starting = () => get().chats.some((c) => c.running && c.runId === null);

  /** Open tabs (saved conversations only), restored at the next start. */
  const persist = () => {
    const { chats, activeChatId } = get();
    const saved = chats.filter((c) => c.sessionId);
    const active = saved.findIndex((c) => c.id === activeChatId);
    try {
      localStorage.setItem(CHATS_KEY, JSON.stringify({ chats: saved.map((c) => ({ sessionId: c.sessionId, title: conversationTitle(c) })), active }));
    } catch {
      /* storage full: tabs are not restored */
    }
  };

  /** Change one conversation; the shown one is mirrored at the top of the store. */
  const update = (id: string, fn: (c: Conversation) => Partial<Conversation>) =>
    set((s) => {
      let shown: Conversation | undefined;
      const chats = s.chats.map((c) => {
        if (c.id !== id) return c;
        const n = { ...c, ...fn(c) };
        if (n.id === s.activeChatId) shown = n;
        return n;
      });
      return shown ? { chats, ...mirror(shown) } : { chats };
    });

  const patchItem = (chatId: string, id: string, patch: Partial<ChatItem>) =>
    update(chatId, (c) => ({ items: c.items.map((i) => (i.id === id ? ({ ...i, ...patch } as ChatItem) : i)) }));

  /** The conversation holding a card. */
  const chatOfItem = (itemId: string) => get().chats.find((c) => c.items.some((i) => i.id === itemId));

  /** Add a conversation tab (closing the oldest idle one past the limit). */
  const addChat = (c: Conversation, activate: boolean) => {
    set((s) => {
      let chats = [...s.chats, c];
      if (chats.length > MAX_CHATS) {
        const drop = chats.find((x) => x.id !== c.id && x.id !== s.activeChatId && !x.running && !x.queued.length);
        if (drop) chats = chats.filter((x) => x !== drop);
      }
      return activate ? { chats, activeChatId: c.id, ...mirror(c) } : { chats };
    });
    persist();
  };

  /** Read a saved conversation's messages into its tab. */
  const loadMessages = async (chatId: string, sessionId: string) => {
    update(chatId, () => ({ loading: true, needsLoad: false }));
    try {
      const [msgs, sessions] = await Promise.all([api.aiSessionMessages(sessionId), api.aiSessions(null)]);
      const sess = sessions.find((s) => s.id === sessionId);
      if (!sess && msgs.length === 0) {
        // Deleted meanwhile (history clean-up): the tab goes away.
        get().closeChat(chatId);
        return;
      }
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
      update(chatId, (c) => ({
        // A question asked while loading stays after the stored messages.
        items: [...items, ...c.items],
        title: sess?.title ?? c.title,
        connectionId: c.connectionId ?? sess?.connection_id ?? get().connectionId,
        tokens: { input: tokens.input + c.tokens.input, output: tokens.output + c.tokens.output },
        loading: false,
      }));
      persist();
    } catch (e) {
      update(chatId, () => ({ loading: false }));
      useStore.getState().toast(toError(e).message, "error");
    }
  };

  const handleAgent = (e: AgentEvent) => {
    const c = byRun(e.run_id);
    if (!c) {
      // Events of a run being started arrive before ai_send returns its id:
      // keep them and replay them once the id is known (dropping "started"
      // lost the session id, so every question began a new conversation).
      if (starting()) early.push(e);
      return;
    }
    const id = c.id;
    // The conversation continues in this session, whichever event says so first.
    if (e.session_id && c.sessionId !== e.session_id) {
      update(id, () => ({ sessionId: e.session_id }));
      persist();
    }
    switch (e.type) {
      case "started":
        break;
      case "text_delta":
        update(id, (c) => {
          const last = c.items[c.items.length - 1];
          if (last?.kind === "assistant" && last.streaming) {
            return { items: [...c.items.slice(0, -1), { ...last, text: last.text + e.text }] };
          }
          return { items: [...c.items, { id: uid(), kind: "assistant", text: e.text, streaming: true }] };
        });
        break;
      case "tool_started":
        if (c.items.some((i) => i.kind === "tool" && i.callId === e.call_id)) break;
        update(id, (c) => ({
          items: [
            ...c.items.map((i) => (i.kind === "assistant" ? { ...i, streaming: false } : i)),
            { id: uid(), kind: "tool", callId: e.call_id, tool: e.tool, args: e.args ?? {}, done: false },
          ],
        }));
        break;
      case "tool_finished":
        update(id, (c) => ({
          items: c.items.map((i) => (i.kind === "tool" && i.callId === e.call_id ? { ...i, done: true, content: e.content, display: e.display } : i)),
        }));
        if (e.tool === "add_knowledge_note" || e.tool === "update_knowledge_note" || e.tool === "index_tables") set((s) => ({ knowledgeVersion: s.knowledgeVersion + 1 }));
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
        update(id, (c) => ({ tokens: { input: c.tokens.input + e.input, output: c.tokens.output + e.output } }));
        break;
      case "metering":
        update(id, (c) => ({ credits: c.credits + e.credits, contextPercent: e.context_percent ?? c.contextPercent }));
        break;
      case "finished":
      case "failed": {
        // The budget moved: refresh it if it is shown.
        if (get().providerId && get().usage[get().providerId!]) void get().loadUsage(get().providerId!, true);
        const providerId = get().providerId;
        update(id, (c) => ({
          running: false,
          runId: null,
          unread: c.id !== get().activeChatId || c.unread,
          items: [
            ...c.items.map((i): ChatItem => {
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
                    providerId: needsProviderSettings(e.error) ? providerId : undefined,
                    retry: c.lastInput ?? undefined,
                  },
                ]
              : []),
          ],
        }));
        set((s) => ({ sessionsVersion: s.sessionsVersion + 1 }));
        // The next question asked meanwhile goes out now. After a failure or
        // Stop it waits for the user (Send now / remove).
        if (e.type === "finished") {
          const [next, ...rest] = chat(id)?.queued ?? [];
          if (next) {
            update(id, () => ({ queued: rest }));
            setTimeout(() => void get().send({ ...next, chatId: id }), 0);
          }
        }
        break;
      }
    }
  };

  const handleRequest = (r: AiUiRequest) => {
    const c = byRun(r.run_id);
    if (!c) {
      if (starting()) {
        earlyRequests.push(r);
        return;
      }
      // Stale run: decline so the backend does not wait for the timeout.
      void api.aiRespond(r.request_id, r.type === "edit_proposal" ? { accepted: false } : { approved: false });
      return;
    }
    const id = c.id;
    const key = c.targetKey;
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
    // A conversation in the background that needs an answer is marked.
    const unread = id !== get().activeChatId && !c.autoApprove;
    if (r.type === "approval_request") {
      if (c.items.some((i) => i.kind === "approval" && i.requestId === r.request_id)) return;
      const itemId = uid();
      update(id, (c) => ({
        unread: c.unread || unread,
        items: [...c.items, { id: itemId, kind: "approval", requestId: r.request_id, tool: r.tool, summary: r.summary, detail: r.detail ?? {}, state: "pending" }],
      }));
      if (c.autoApprove) void get().respondApproval(itemId, true, undefined, true);
      return;
    }
    if (c.items.some((i) => i.kind === "edit" && i.requestId === r.request_id)) return;
    const target = r.proposal.mode === "new_tab" ? null : key;
    const editId = uid();
    update(id, (c) => ({
      unread: c.unread || unread,
      items: [
        ...c.items,
        {
          id: editId,
          kind: "edit",
          requestId: r.request_id,
          targetKey: target,
          proposal: r.proposal,
          before: target ? (editorBridge.snapshot(target)?.selection ?? editorBridge.snapshot(target)?.sql ?? "") : "",
          state: "pending",
        },
      ],
    }));
    if (c.autoApprove) void get().respondEdit(editId, true, true);
  };

  // Conversations open at the last start: their tabs come back, and their
  // messages are read when they are shown.
  const restored = (() => {
    try {
      const v = JSON.parse(localStorage.getItem(CHATS_KEY) ?? "null") as { chats?: { sessionId?: unknown; title?: unknown }[]; active?: number } | null;
      const chats = (v?.chats ?? [])
        .filter((x) => typeof x?.sessionId === "string")
        .slice(0, MAX_CHATS)
        .map((x): Conversation => ({ ...emptyConversation(), sessionId: x.sessionId as string, title: typeof x.title === "string" ? x.title : null, needsLoad: true }));
      return { chats, active: chats[v?.active ?? -1] ?? chats[0] };
    } catch {
      return { chats: [], active: undefined };
    }
  })();
  const first = restored.active ?? emptyConversation();
  const initialChats = restored.chats.length ? restored.chats : [first];

  return {
    open: localStorage.getItem("db.ai.open") === "1",
    view: "chat",
    chats: initialChats,
    activeChatId: first.id,
    ...mirror(first),
    providers: [],
    providerId: null,
    model: null,
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
    resend: (itemId) => {
      const c = chatOfItem(itemId);
      const it = c?.items.find((i) => i.id === itemId);
      if (!c || it?.kind !== "error" || !it.retry) return;
      const retry = it.retry;
      update(c.id, (c) => ({ items: c.items.filter((i) => i.id !== itemId) }));
      void get().send({ ...retry, chatId: c.id });
    },
    markReauthed: (providerId) => {
      for (const c of get().chats)
        if (c.items.some((i) => i.kind === "error" && i.retry))
          update(c.id, (c) => ({
            items: c.items.map((i) => (i.kind === "error" && i.retry && (i.providerId === providerId || i.providerId === null) ? { ...i, reauthed: true } : i)),
          }));
    },
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
      // The shown restored conversation is read now; the others when opened.
      const shown = chat(get().activeChatId);
      if (shown?.needsLoad && shown.sessionId) void loadMessages(shown.id, shown.sessionId);
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
      const c = chat(input.chatId) ?? chat(get().activeChatId);
      if (!c) return;
      const id = c.id;
      const { chatId: _ignored, ...plain } = input;
      void _ignored;
      if (c.running) {
        // Asked while this conversation works: sent when its answer is done.
        update(id, (c) => ({ queued: [...c.queued, { ...plain, id: uid() }] }));
        return;
      }
      if (!get().providerId) {
        st.toast("Add an AI provider first", "info");
        st.setSettingsOpen(true);
        return;
      }
      const key = targetKey !== undefined ? targetKey : defaultTargetKey();
      const conn = connectionId ?? connectionForKey(key) ?? c.connectionId ?? get().connectionId;
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
      const sessionId = c.sessionId;
      update(id, (c) => ({
        items: [...c.items, { id: uid(), kind: "user", text: message, mode }],
        running: true,
        runId: null,
        lastInput: plain,
        targetKey: key,
        connectionId: conn,
        unread: false,
      }));
      set({ open: true, view: "chat" });
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
        // Closed while starting: stop the run it started.
        if (!chat(id)) {
          void api.aiCancel(runId);
          return;
        }
        update(id, () => ({ runId }));
        // Replay what this run sent before its id was known.
        const evs = early.filter((x) => x.run_id === runId);
        const reqs = earlyRequests.filter((x) => x.run_id === runId);
        early = early.filter((x) => x.run_id !== runId);
        earlyRequests = earlyRequests.filter((x) => x.run_id !== runId);
        if (!starting()) {
          early = [];
          earlyRequests = [];
        }
        for (const x of evs) handleAgent(x);
        for (const x of reqs) handleRequest(x);
      } catch (e) {
        const err = toError(e);
        const providerId = get().providerId;
        update(id, (c) => ({
          running: false,
          items: [...c.items, { id: uid(), kind: "error", text: err.message, providerId: needsProviderSettings(err) ? providerId : undefined, retry: plain }],
        }));
      }
    },

    cancel: async (chatId) => {
      const id = chat(chatId ?? get().activeChatId)?.runId;
      if (id) await api.aiCancel(id);
    },

    newChat: (opts = {}) => {
      const activate = opts.activate ?? true;
      const shown = chat(get().activeChatId);
      if (shown && isBlank(shown)) {
        if (activate) set({ view: "chat" });
        return shown.id;
      }
      // A new conversation keeps the connection the assistant was on.
      const c = emptyConversation(shown?.connectionId ?? null);
      addChat(c, activate);
      if (activate) set({ view: "chat" });
      return c.id;
    },

    switchChat: (id) => {
      const c = chat(id);
      if (!c) return;
      set({ activeChatId: id, view: "chat" });
      update(id, () => ({ unread: false }));
      if (c.needsLoad && c.sessionId) void loadMessages(id, c.sessionId);
      persist();
    },

    closeChat: (id) => {
      const c = chat(id);
      if (!c) return;
      if (c.runId) void api.aiCancel(c.runId).catch(() => {});
      // Nobody will answer its open requests: decline them now.
      for (const i of c.items) {
        if (i.kind === "approval" && i.state === "pending") void api.aiRespond(i.requestId, { approved: false }).catch(() => {});
        if (i.kind === "edit" && i.state === "pending") void api.aiRespond(i.requestId, { accepted: false }).catch(() => {});
      }
      const { chats, activeChatId } = get();
      const at = chats.findIndex((x) => x.id === id);
      let rest = chats.filter((x) => x.id !== id);
      if (!rest.length) rest = [emptyConversation(c.connectionId)];
      const next = activeChatId === id ? rest[Math.min(at, rest.length - 1)] : chat(activeChatId)!;
      set({ chats: rest, activeChatId: next.id, ...mirror(next) });
      if (activeChatId === id && next.needsLoad && next.sessionId) void loadMessages(next.id, next.sessionId);
      persist();
    },

    forgetSession: (sessionId) => {
      for (const c of get().chats) if (c.sessionId === sessionId) get().closeChat(c.id);
    },

    removeQueued: (qid) => {
      const c = get().chats.find((c) => c.queued.some((q) => q.id === qid));
      if (c) update(c.id, (c) => ({ queued: c.queued.filter((q) => q.id !== qid) }));
    },
    sendQueued: (qid) => {
      const c = get().chats.find((c) => c.queued.some((q) => q.id === qid));
      const q = c?.queued.find((x) => x.id === qid);
      if (!c || !q || c.running) return;
      update(c.id, (c) => ({ queued: c.queued.filter((x) => x.id !== qid) }));
      void get().send({ ...q, chatId: c.id });
    },

    loadSession: async (sessionId) => {
      // Already open: show its tab.
      const open = get().chats.find((c) => c.sessionId === sessionId);
      if (open) return get().switchChat(open.id);
      const shown = chat(get().activeChatId);
      let id: string;
      if (shown && isBlank(shown)) {
        id = shown.id;
        update(id, () => ({ sessionId }));
        set({ view: "chat" });
      } else {
        const c = { ...emptyConversation(), sessionId };
        addChat(c, true);
        set({ view: "chat" });
        id = c.id;
      }
      await loadMessages(id, sessionId);
    },

    setAutoApprove: (on) => {
      const id = get().activeChatId;
      update(id, () => ({ autoApprove: on }));
      if (!on) return;
      // What is already waiting is approved too.
      for (const i of chat(id)?.items ?? []) {
        if (i.kind === "approval" && i.state === "pending") void get().respondApproval(i.id, true, undefined, true);
        if (i.kind === "edit" && i.state === "pending") void get().respondEdit(i.id, true, true);
      }
    },

    respondApproval: async (itemId, approved, editedSql, auto) => {
      const c = chatOfItem(itemId);
      const item = c?.items.find((i) => i.id === itemId);
      if (!c || !item || item.kind !== "approval" || item.state !== "pending") return;
      const detail = editedSql !== undefined ? { ...item.detail, sql: editedSql } : item.detail;
      patchItem(c.id, itemId, { state: approved ? "approved" : "denied", detail, auto: !!auto } as Partial<ChatItem>);
      await api.aiRespond(item.requestId, { approved, detail });
    },

    respondEdit: async (itemId, accepted, auto) => {
      const c = chatOfItem(itemId);
      const item = c?.items.find((i) => i.id === itemId);
      if (!c || !item || item.kind !== "edit" || item.state !== "pending") return;
      const chatConn = c.connectionId;
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
            st.newTab({ title: item.proposal.title || "AI query", sql: withDuckdbHeader(item.proposal.sql), connection_id: results ?? chatConn });
          } else {
            st.newTab({ title: item.proposal.title || "AI query", sql: item.proposal.sql, connection_id: chatConn });
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
      patchItem(c.id, itemId, { state: ok ? "accepted" : "rejected", auto: ok && !!auto } as Partial<ChatItem>);
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
