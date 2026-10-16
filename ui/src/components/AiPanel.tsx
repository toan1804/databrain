import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  AlertCircle,
  BookOpen,
  Bot,
  Check,
  ChevronDown,
  ChevronRight,
  Clock,
  Copy,
  Database,
  FileCode2,
  History,
  Lightbulb,
  Loader2,
  MessageSquarePlus,
  Play,
  RefreshCw,
  Send,
  Settings2,
  ShieldAlert,
  Sparkles,
  Square,
  Trash2,
  Workflow,
  Wrench,
  X,
  ListChecks,
  FileDown,
  FileUp,
} from "lucide-react";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { api, toError } from "../lib/api";
import type { AiMode, AiSessionRecord, ConnectionView, KnNote, KnObject, KnowledgeView, NotesImportPreview, ResultInfo } from "../lib/types";
import { emptyView, formatCount, lineDiff, placeComboList, relativeTime, sqlPreview } from "../lib/util";
import { useStore } from "../store";
import { connectionForKey, useAi, visibleItems, workingStatus, type ChatItem } from "../aiStore";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { exportFileName } from "../lib/notesImport";
import { ImportNotesDialog } from "./ImportNotesDialog";
import { NoteTargetInput, type TargetState } from "./NoteTargetInput";
import { editorBridge } from "../editorBridge";
import { Markdown } from "./Markdown";
import { extractMentions, mentionToken, resultsConnection } from "../outputs";
import { ResultGrid } from "./ResultGrid";
import { ComboInput, ConnDot } from "./ui";
import { UsageFooter } from "./ProviderUsage";

export function AiPanel() {
  const view = useAi((s) => s.view);
  const setView = useAi((s) => s.setView);
  const setOpen = useAi((s) => s.setOpen);
  const newChat = useAi((s) => s.newChat);
  const setSettingsOpen = useStore((s) => s.setSettingsOpen);

  const tab = (v: typeof view, icon: React.ReactNode, label: string) => (
    <button
      role="tab"
      aria-selected={view === v}
      onClick={() => setView(v)}
      className={`flex h-7 items-center gap-1.5 rounded-md px-2 text-[12px] ${view === v ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
    >
      {icon}
      {label}
    </button>
  );

  return (
    <div className="flex h-full min-w-0 flex-col border-l border-line bg-panel">
      <div className="flex h-10 shrink-0 items-center gap-0.5 border-b border-line px-2" role="tablist">
        {tab("chat", <Sparkles size={13} />, "Assistant")}
        {tab("history", <History size={13} />, "History")}
        {tab("knowledge", <BookOpen size={13} />, "Knowledge")}
        <div className="ml-auto flex items-center">
          <button className="icon-btn" title="New chat" aria-label="New chat" onClick={newChat}>
            <MessageSquarePlus size={14} />
          </button>
          <button className="icon-btn" title="AI settings" aria-label="AI settings" onClick={() => setSettingsOpen(true)}>
            <Settings2 size={14} />
          </button>
          <button className="icon-btn" title="Close (⌘L)" aria-label="Close assistant" onClick={() => setOpen(false)}>
            <X size={14} />
          </button>
        </div>
      </div>
      {view === "chat" && <Chat />}
      {view === "history" && <Sessions />}
      {view === "knowledge" && <Knowledge />}
    </div>
  );
}

// ------------------------------------------------------------------ chat

const NO_MODELS: string[] = [];

const QUICK: { mode: AiMode; label: string; prompt: string; icon: React.ReactNode }[] = [
  { mode: "generate", label: "Write SQL", prompt: "", icon: <FileCode2 size={12} /> },
  { mode: "explain", label: "Explain query", prompt: "Explain what the query in my editor does.", icon: <Lightbulb size={12} /> },
  { mode: "fix_error", label: "Fix error", prompt: "Fix the error in my query.", icon: <Wrench size={12} /> },
  { mode: "analyze_result", label: "Analyze result", prompt: "Analyze the current result and summarize the key findings.", icon: <Sparkles size={12} /> },
];

function Chat() {
  const items = useAi((s) => s.items);
  const running = useAi((s) => s.running);
  const queued = useAi((s) => s.queued);
  const removeQueued = useAi((s) => s.removeQueued);
  const sendQueued = useAi((s) => s.sendQueued);
  const send = useAi((s) => s.send);
  const cancel = useAi((s) => s.cancel);
  const providers = useAi((s) => s.providers);
  const providerId = useAi((s) => s.providerId);
  const model = useAi((s) => s.model);
  const setProvider = useAi((s) => s.setProvider);
  const aiConn = useAi((s) => s.connectionId);
  const connections = useStore((s) => s.connections);
  const activeTab = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId));
  const setSettingsOpen = useStore((s) => s.setSettingsOpen);
  const [text, setText] = useState("");
  const [mode, setMode] = useState<AiMode>("chat");
  const models = useAi((s) => (providerId ? s.models[providerId] : undefined)) ?? NO_MODELS;
  const outputs = useStore((s) => s.outputs);
  const [mention, setMention] = useState<{ start: number; query: string; index: number } | null>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLTextAreaElement>(null);

  // A job tab has no connection of its own: the assistant works on jobs
  // there, on the last connection it used, else the Results (DuckDB) one.
  const jobTab = activeTab?.job_id ? { id: activeTab.job_id, name: activeTab.title } : null;
  // Sidebar → Jobs with no connection in the editor: ask about jobs the same way.
  const jobsPanel = useStore((s) => s.sidebarPanel === "jobs");
  const [resultsId, setResultsId] = useState<string | null>(null);
  const editorConn = jobTab ? null : (connectionForKey(activeTab?.id) ?? aiConn ?? activeTab?.connection_id ?? null);
  const needsJobConn = (!!jobTab || (jobsPanel && !editorConn)) && !(aiConn && connections.some((c) => c.id === aiConn && c.ai_policy?.ai_enabled !== false));
  useEffect(() => {
    if (needsJobConn && !resultsId) void resultsConnection().then(setResultsId).catch(() => {});
  }, [needsJobConn, resultsId]);
  const also = useAi((s) => s.alsoConnections);
  const firstAlso = also.find((id) => connections.some((c) => c.id === id && c.ai_policy?.ai_enabled !== false)) ?? null;
  // No editor connection: the first connection chosen in the picker leads.
  const connId = (jobTab || (jobsPanel && !editorConn) ? (needsJobConn ? (firstAlso ?? resultsId) : aiConn) : editorConn) ?? firstAlso;
  const conn = connections.find((c) => c.id === connId);
  const provider = providers.find((p) => p.id === providerId);

  useEffect(() => {
    scroller.current?.scrollTo({ top: scroller.current.scrollHeight });
  }, [items]);

  useEffect(() => {
    const onPrefill = (e: Event) => {
      const d = (e as CustomEvent<{ text: string; mode: AiMode; append?: boolean; handled?: boolean }>).detail;
      if (d.handled) return;
      d.handled = true;
      setText((t) => (d.append && t.trim() ? `${t.replace(/\s*$/, " ")}${d.text}` : d.text));
      setMode(d.mode);
      setTimeout(() => input.current?.focus(), 0);
    };
    window.addEventListener("db:ai-prefill", onPrefill);
    return () => window.removeEventListener("db:ai-prefill", onPrefill);
  }, []);

  useEffect(() => {
    // Shared list: loaded once per provider (and kept), not per render or pick.
    if (providerId) void useAi.getState().loadModels(providerId).catch(() => {});
  }, [providerId]);

  const suggestions = useMemo(() => {
    if (!mention) return [];
    const q = mention.query.toLowerCase();
    return outputs
      .filter((o) => o.state !== "evicted")
      .filter((o) => !q || o.handle.startsWith(q) || (o.name ?? "").toLowerCase().includes(q) || o.connection_name.toLowerCase().includes(q))
      .slice(0, 8);
  }, [mention, outputs]);

  const updateMention = (value: string, caret: number) => {
    const before = value.slice(0, caret);
    const m = before.match(/(^|[\s(,])@([A-Za-z_]\w*)?$/);
    setMention(m ? { start: caret - (m[2]?.length ?? 0) - 1, query: m[2] ?? "", index: 0 } : null);
  };

  const pickMention = (o: (typeof outputs)[number]) => {
    if (!mention) return;
    const el = input.current;
    const caret = el?.selectionStart ?? text.length;
    const token = `${mentionToken(o)} `;
    const next = text.slice(0, mention.start) + token + text.slice(caret);
    setText(next);
    setMention(null);
    setTimeout(() => {
      el?.focus();
      const pos = mention.start + token.length;
      el?.setSelectionRange(pos, pos);
    }, 0);
  };

  const submit = (msg = text, m = mode) => {
    // While the assistant works the question waits and is sent when it finishes.
    if (!msg.trim()) return;
    const mentions = extractMentions(msg, outputs);
    const context = {
      ...(mentions.length ? { mentions } : {}),
      ...(jobTab ? { open_job_id: jobTab.id, open_job_name: jobTab.name } : {}),
    };
    void send({ message: msg.trim(), mode: m, connectionId: connId, targetKey: jobTab ? null : undefined, context: Object.keys(context).length ? context : undefined });
    setText("");
    setMode("chat");
  };

  const aiDisabled = conn && conn.ai_policy && !conn.ai_policy.ai_enabled;

  return (
    <>
      <div ref={scroller} className="min-h-0 flex-1 space-y-3 overflow-auto p-3">
        {providers.length === 0 && (
          <div className="rounded-lg border border-line bg-panel-2 p-3 text-[12.5px]">
            <div className="mb-1 flex items-center gap-2 font-semibold">
              <Bot size={15} /> Set up an AI provider
            </div>
            <p className="text-muted">
              Use OpenAI, Anthropic, Gemini, Azure OpenAI, OpenRouter (browser sign-in) or a local model (Ollama / LM Studio).
            </p>
            <button className="btn-primary mt-2" onClick={() => setSettingsOpen(true)}>
              Add provider
            </button>
          </div>
        )}
        {items.length === 0 && providers.length > 0 && (
          <div className="px-2 pt-6 text-center text-[12.5px] text-muted">
            <Sparkles size={24} className="mx-auto mb-2 opacity-60" />
            Ask about your data. The assistant reads schema metadata, can run read-only queries (with your approval) and
            write SQL into the editor.
            <div className="mt-4 grid grid-cols-2 gap-1.5 text-left">
              {QUICK.map((q) => (
                <button
                  key={q.mode}
                  className="flex items-center gap-1.5 rounded-lg border border-line px-2 py-1.5 text-[12px] text-fg hover:bg-hover"
                  onClick={() => (q.prompt ? submit(q.prompt, q.mode) : (setMode(q.mode), input.current?.focus()))}
                >
                  {q.icon}
                  {q.label}
                </button>
              ))}
            </div>
          </div>
        )}
        {visibleItems(items).map((it) => (
          <ChatItemView key={it.id} item={it} />
        ))}
      </div>

      <div className="shrink-0 border-t border-line p-2">
        {running && <WorkingBar items={items} onStop={() => void cancel()} />}
        {queued.length > 0 && (
          <div className="mb-1.5 space-y-1" aria-label="Waiting questions">
            {queued.map((q, i) => (
              <div key={q.id} className="flex items-center gap-1.5 rounded-md border border-dashed border-line bg-panel-2 px-2 py-1 text-[11.5px]">
                <Clock size={11} className="shrink-0 text-muted" />
                <span className="min-w-0 flex-1 truncate" title={q.message}>
                  {q.message}
                </span>
                <span className="shrink-0 text-muted">{running ? (i === 0 ? "sends when the assistant finishes" : "waiting") : "not sent"}</span>
                {!running && i === 0 && (
                  <button className="shrink-0 text-accent hover:underline" onClick={() => sendQueued(q.id)}>
                    Send now
                  </button>
                )}
                <button className="shrink-0 text-muted hover:text-fg" aria-label="Remove waiting question" title="Don't send" onClick={() => removeQueued(q.id)}>
                  <X size={11} />
                </button>
              </div>
            ))}
          </div>
        )}
        {aiDisabled && (
          <div className="mb-1.5 flex items-center gap-1.5 rounded-md bg-warning/10 px-2 py-1 text-[11.5px] text-warning">
            <ShieldAlert size={12} /> AI is disabled for this connection (Edit connection → AI).
          </div>
        )}
        {mode !== "chat" && (
          <div className="mb-1.5 flex items-center gap-1 text-[11.5px]">
            <span className="rounded bg-accent/15 px-1.5 py-0.5 text-accent">{QUICK.find((q) => q.mode === mode)?.label ?? mode}</span>
            <button className="text-muted hover:text-fg" aria-label="Clear mode" onClick={() => setMode("chat")}>
              <X size={11} />
            </button>
          </div>
        )}
        <div className="relative rounded-lg border border-line bg-panel-2 focus-within:border-accent">
          {mention && suggestions.length > 0 && (
            <div className="absolute bottom-full left-0 z-20 mb-1 w-full overflow-hidden rounded-lg border border-line bg-panel shadow-xl" role="listbox" aria-label="Outputs">
              <div className="px-2 py-1 text-[10.5px] uppercase tracking-wide text-muted">Mention an output</div>
              {suggestions.map((o, i) => (
                <button
                  key={o.handle}
                  role="option"
                  aria-selected={i === mention.index}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    pickMention(o);
                  }}
                  className={`flex w-full items-center gap-2 px-2 py-1 text-left text-[12px] ${i === mention.index ? "bg-hover" : ""}`}
                >
                  <span className="font-mono">@{o.name ?? o.handle}</span>
                  {o.name && <span className="font-mono text-[10.5px] text-muted">{o.handle}</span>}
                  <span className="ml-auto truncate text-[10.5px] text-muted">
                    {o.connection_name} · {o.rows.toLocaleString()} rows
                  </span>
                </button>
              ))}
            </div>
          )}
          <textarea
            ref={input}
            rows={3}
            value={text}
            aria-label="Message the assistant"
            placeholder={
              !conn
                ? jobTab
                  ? "Preparing the assistant for jobs…"
                  : "Choose a connection in the editor first"
                : running
                  ? "The assistant is working… type your next question, it is sent when it finishes"
                  : jobTab
                    ? `Ask about the job "${jobTab.name}": explain it, fix its errors, add a step…  (↵ send)`
                    : `Ask about ${conn.name}… @ to mention an output  (↵ send, ⇧↵ newline)`
            }
            className="block w-full resize-none bg-transparent px-2.5 py-2 text-[13px] outline-none"
            onChange={(e) => {
              setText(e.target.value);
              updateMention(e.target.value, e.target.selectionStart ?? e.target.value.length);
            }}
            onBlur={() => setTimeout(() => setMention(null), 100)}
            onKeyDown={(e) => {
              if (mention && suggestions.length > 0) {
                if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                  e.preventDefault();
                  const d = e.key === "ArrowDown" ? 1 : -1;
                  setMention({ ...mention, index: (mention.index + d + suggestions.length) % suggestions.length });
                  return;
                }
                if (e.key === "Enter" || e.key === "Tab") {
                  e.preventDefault();
                  pickMention(suggestions[mention.index]);
                  return;
                }
                if (e.key === "Escape") {
                  setMention(null);
                  return;
                }
              }
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                submit();
              }
            }}
          />
          <div className="flex items-center gap-1.5 px-2 pb-1.5">
            <ConnectionPicker current={conn ?? null} />
            {jobTab && (
              <span className="flex min-w-0 shrink items-center gap-1 rounded bg-accent/10 px-1 text-[11px] text-accent" title="The assistant knows which job is open">
                <Workflow size={10} className="shrink-0" />
                <span className="truncate">{jobTab.name}</span>
              </span>
            )}
            <select
              className="ml-auto h-6 max-w-[110px] rounded border border-line bg-panel px-1 text-[11px] outline-none"
              aria-label="AI provider"
              value={providerId ?? ""}
              onChange={(e) => setProvider(e.target.value || null)}
            >
              {providers.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
            <ComboInput
              className="h-6 w-[120px] rounded border border-line bg-panel px-1 text-[11px] outline-none"
              aria-label="Model"
              placeholder={provider?.config.default_model ?? "model"}
              value={model ?? ""}
              options={models}
              onChange={(v) => setProvider(providerId, v || null)}
            />
            {running ? (
              <button
                className="btn-ghost h-6 border border-line px-2 py-0"
                onClick={() => submit()}
                disabled={!text.trim() || !conn}
                aria-label="Send when the assistant finishes"
                title="Send when the assistant finishes (↵)"
              >
                <Clock size={12} />
              </button>
            ) : (
              <button className="btn-primary h-6 px-2 py-0" onClick={() => submit()} disabled={!text.trim() || !conn} aria-label="Send">
                <Send size={12} />
              </button>
            )}
          </div>
        </div>
        <UsageFooter />
      </div>
    </>
  );
}

/** Status line while the assistant works: what it is doing, for how long, and Stop. */
function WorkingBar({ items, onStop }: { items: ChatItem[]; onStop: () => void }) {
  const [started] = useState(() => Date.now());
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const st = workingStatus(items);
  const secs = Math.max(0, Math.round((now - started) / 1000));
  return (
    <div
      className={`mb-1.5 flex items-center gap-2 rounded-md px-2 py-1 text-[11.5px] ${st.waitingForYou ? "bg-warning/10 text-warning" : "bg-accent/10 text-accent"}`}
      role="status"
      aria-live="polite"
    >
      <Loader2 size={12} className={st.waitingForYou ? "" : "animate-spin"} />
      <span className="min-w-0 flex-1 truncate">{st.text}</span>
      <span className="shrink-0 tabular-nums opacity-75">{secs < 60 ? `${secs} s` : `${Math.floor(secs / 60)} min ${secs % 60} s`}</span>
      <button className="btn-danger h-5 shrink-0 px-1.5 py-0 text-[11px]" onClick={onStop} aria-label="Stop the assistant">
        <Square size={9} fill="currentColor" /> Stop
      </button>
    </div>
  );
}

/** SQL code block with copy / insert / run actions. */
export function SqlBlock({ code, lang }: { code: string; lang: string }) {
  const st = useStore();
  const isSql = !lang || lang === "sql";
  const target = editorBridge.focused() ?? st.activeTabId;
  return (
    <div className="overflow-hidden rounded-md border border-line bg-panel-2">
      <pre className="max-h-72 overflow-auto p-2 font-mono text-[12px] leading-snug select-text">{code}</pre>
      <div className="flex gap-1 border-t border-line px-1.5 py-1">
        <button className="btn-ghost h-6 px-1.5 py-0 text-[11px]" onClick={() => void writeText(code).then(() => st.toast("Copied", "success"))}>
          <Copy size={11} /> Copy
        </button>
        {isSql && (
          <>
            <button className="btn-ghost h-6 px-1.5 py-0 text-[11px]" onClick={() => editorBridge.insert(target, code)}>
              <FileCode2 size={11} /> Insert
            </button>
            <button
              className="btn-ghost h-6 px-1.5 py-0 text-[11px]"
              onClick={() => st.newTab({ title: "AI query", sql: code, connection_id: useAi.getState().connectionId })}
            >
              <Play size={11} /> Open in tab
            </button>
          </>
        )}
      </div>
    </div>
  );
}

function ChatItemView({ item }: { item: ChatItem }) {
  const respondApproval = useAi((s) => s.respondApproval);
  const respondEdit = useAi((s) => s.respondEdit);
  switch (item.kind) {
    case "user":
      return (
        <div className="ml-6 rounded-lg bg-accent/12 px-3 py-2 text-[13px] whitespace-pre-wrap select-text">
          {item.text.split(/((?:^|(?<=[\s(,]))@[A-Za-z_]\w*)/).map((part, i) =>
            part.startsWith("@") ? (
              <span key={i} className="rounded bg-accent/20 px-0.5 font-mono text-[12px] text-accent">
                {part}
              </span>
            ) : (
              part
            ),
          )}
        </div>
      );
    case "assistant":
      return (
        <div className="select-text">
          <Markdown text={item.text} code={(c, l, k) => <SqlBlock key={k} code={c} lang={l} />} />
          {item.streaming && <span className="ml-0.5 inline-block h-3 w-1.5 animate-pulse bg-accent align-middle" />}
        </div>
      );
    case "error":
      return (
        <div className="rounded-lg border border-danger/40 bg-danger/10 p-2 text-[12.5px]" role="alert">
          <div className="flex items-start gap-2">
            <AlertCircle size={14} className="mt-px shrink-0 text-danger" />
            <span className="break-words">{item.text}</span>
          </div>
          {item.reauthed && <div className="mt-1.5 text-[11.5px] text-success">Signed in again. Send the question again?</div>}
          {(item.providerId !== undefined || item.retry) && (
            <div className="mt-1.5 flex flex-wrap gap-1.5">
              {item.retry && (
                <button
                  className={`${item.reauthed ? "btn-primary" : "btn-ghost border border-line"} py-1 text-[12px]`}
                  onClick={() => useAi.getState().resend(item.id)}
                  title={item.retry.message}
                >
                  <RefreshCw size={12} /> Resend question
                </button>
              )}
              {item.providerId !== undefined && !item.reauthed && (
                <button className="btn-ghost border border-line py-1 text-[12px]" onClick={() => useAi.getState().openProviderSettings(item.providerId)}>
                  <Settings2 size={12} /> Open provider settings to sign in again
                </button>
              )}
            </div>
          )}
        </div>
      );
    case "tool":
      return <ToolCard item={item} />;
    case "approval":
      return <ApprovalCard item={item} onRespond={respondApproval} />;
    case "edit":
      return <EditCard item={item} onRespond={respondEdit} />;
  }
}

const TOOL_LABELS: Record<string, string> = {
  search_schema: "Searched schema",
  describe_table: "Described table",
  list_tables: "Listed tables",
  get_sample_rows: "Sampled rows",
  run_query: "Ran query",
  query_result: "Queried result",
  result_summary: "Summarized result",
  add_knowledge_note: "Added knowledge note",
  update_knowledge_note: "Updated knowledge note",
  list_knowledge_notes: "Read notes & glossary",
  save_query: "Saved query",
  get_editor: "Read editor",
  write_editor: "Proposed editor change",
  list_outputs: "Listed outputs",
  query_outputs: "Queried outputs (DuckDB)",
  list_connections: "Listed connections",
  list_jobs: "Listed jobs",
  get_job: "Read job",
  create_job: "Created job",
  update_job: "Updated job",
  run_job: "Ran job",
};

function ToolCard({ item }: { item: Extract<ChatItem, { kind: "tool" }> }) {
  const [open, setOpen] = useState(false);
  const arg = (k: string) => (typeof item.args[k] === "string" ? (item.args[k] as string) : undefined);
  const subject = arg("sql") ?? arg("table") ?? arg("query") ?? arg("note") ?? arg("name") ?? "";
  const results = useMemo(() => {
    const d = item.display as { statements?: { result?: ResultInfo }[] } | undefined;
    return (d?.statements ?? []).map((s) => s.result).filter((r): r is ResultInfo => !!r);
  }, [item.display]);
  return (
    <div className="rounded-lg border border-line text-[12px]">
      <button className="flex w-full items-center gap-1.5 px-2 py-1.5 text-left" onClick={() => setOpen(!open)} aria-expanded={open}>
        {item.done ? <Check size={12} className="shrink-0 text-success" /> : <Loader2 size={12} className="shrink-0 animate-spin text-accent" />}
        <span className="shrink-0 font-medium">{TOOL_LABELS[item.tool] ?? item.tool}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-muted">{sqlPreview(subject, 80)}</span>
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
      </button>
      {results[0] && <MiniGrid info={results[0]} />}
      {open && (
        <div className="space-y-1.5 border-t border-line p-2">
          {arg("sql") && <pre className="overflow-x-auto whitespace-pre-wrap font-mono text-[11.5px] select-text">{arg("sql")}</pre>}
          {item.content && (
            <pre className="max-h-48 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-muted select-text">{item.content}</pre>
          )}
        </div>
      )}
    </div>
  );
}

function MiniGrid({ info }: { info: ResultInfo }) {
  const [view, setView] = useState(emptyView);
  return (
    <div className="border-t border-line" style={{ height: Math.min(240, 36 + Math.max(1, info.total_rows) * 30) }}>
      <ResultGrid
        info={info}
        view={view}
        onViewChange={setView}
        onViewRows={() => {}}
        onHeaderMenu={() => {}}
        findMatches={[]}
        findCurrent={0}
        dialect={undefined}
      />
    </div>
  );
}

function ApprovalCard({
  item,
  onRespond,
}: {
  item: Extract<ChatItem, { kind: "approval" }>;
  onRespond: (id: string, approved: boolean, sql?: string) => void;
}) {
  const initialSql = typeof item.detail.sql === "string" ? item.detail.sql : undefined;
  const [sql, setSql] = useState(initialSql ?? "");
  const pending = item.state === "pending";
  return (
    <div className={`rounded-lg border p-2 text-[12.5px] ${pending ? "border-warning/50 bg-warning/5" : "border-line"}`}>
      <div className="mb-1 flex items-center gap-1.5 font-medium">
        <ShieldAlert size={13} className="text-warning" />
        {item.tool === "run_query" ? "Run this query?" : item.tool === "create_job" ? "Create this job?" : item.tool === "update_job" ? "Change this job?" : item.tool === "run_job" ? "Run this job?" : `Allow ${item.tool}?`}
        {!pending && (
          <span className={`ml-auto text-[11px] ${item.state === "approved" ? "text-success" : "text-muted"}`}>
            {item.state === "approved" ? "Approved" : "Declined"}
          </span>
        )}
      </div>
      <div className="mb-1.5 text-[11.5px] text-muted">{item.summary}</div>
      {initialSql !== undefined &&
        (pending ? (
          <textarea
            className="field min-h-[70px] font-mono text-[11.5px]"
            value={sql}
            aria-label="SQL to run (editable)"
            onChange={(e) => setSql(e.target.value)}
          />
        ) : (
          <pre className="whitespace-pre-wrap font-mono text-[11.5px] select-text">{sql}</pre>
        ))}
      {initialSql === undefined && typeof item.detail.preview === "string" && (
        <pre className="max-h-64 overflow-auto whitespace-pre-wrap rounded border border-line bg-panel-2 p-1.5 font-mono text-[11px] select-text">{item.detail.preview}</pre>
      )}
      {initialSql === undefined && typeof item.detail.preview !== "string" && (
        <pre className="max-h-32 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-muted">{JSON.stringify(item.detail, null, 2)}</pre>
      )}
      {pending && (
        <div className="mt-2 flex justify-end gap-1.5">
          <button className="btn-ghost py-1" onClick={() => onRespond(item.id, false)}>
            Deny
          </button>
          <button className="btn-primary py-1" onClick={() => onRespond(item.id, true, initialSql !== undefined ? sql : undefined)}>
            <Play size={12} /> {item.tool === "run_query" ? "Run" : "Allow"}
          </button>
        </div>
      )}
    </div>
  );
}

function EditCard({ item, onRespond }: { item: Extract<ChatItem, { kind: "edit" }>; onRespond: (id: string, ok: boolean) => void }) {
  const diff = useMemo(() => lineDiff(item.before, item.proposal.sql), [item.before, item.proposal.sql]);
  const pending = item.state === "pending";
  const target =
    item.proposal.mode === "new_tab" || !item.targetKey
      ? "a new tab"
      : item.proposal.mode === "replace_all"
        ? "the whole editor"
        : item.proposal.mode === "insert_at_cursor"
          ? "the cursor position"
          : "the selection";
  return (
    <div className={`overflow-hidden rounded-lg border text-[12px] ${pending ? "border-accent/60" : "border-line"}`}>
      <div className="flex items-center gap-1.5 px-2 py-1.5 font-medium">
        <FileCode2 size={13} className="text-accent" />
        {item.proposal.title || "Proposed SQL"}
        <span className="text-[11px] font-normal text-muted">→ {target}</span>
        {!pending && (
          <span className={`ml-auto text-[11px] ${item.state === "accepted" ? "text-success" : "text-muted"}`}>
            {item.state === "accepted" ? "Applied" : "Rejected"}
          </span>
        )}
      </div>
      <div className="max-h-72 overflow-auto border-t border-line font-mono text-[11.5px] leading-snug select-text">
        {(item.before && item.proposal.mode !== "insert_at_cursor" ? diff : diff.filter((d) => d.op !== "del")).map((d, i) => (
          <div
            key={i}
            className={`whitespace-pre-wrap px-2 ${d.op === "add" ? "bg-success/12" : d.op === "del" ? "bg-danger/12 text-muted line-through" : ""}`}
          >
            <span className="mr-1.5 inline-block w-2 select-none text-muted">{d.op === "add" ? "+" : d.op === "del" ? "−" : " "}</span>
            {d.text || " "}
          </div>
        ))}
      </div>
      {pending && (
        <div className="flex justify-end gap-1.5 border-t border-line p-1.5">
          <button className="btn-ghost py-1" onClick={() => onRespond(item.id, false)}>
            Reject
          </button>
          <button className="btn-primary py-1" onClick={() => onRespond(item.id, true)}>
            <Check size={12} /> Accept
          </button>
        </div>
      )}
    </div>
  );
}

// ------------------------------------------------------------------ sessions

function Sessions() {
  const version = useAi((s) => s.sessionsVersion);
  const loadSession = useAi((s) => s.loadSession);
  const current = useAi((s) => s.sessionId);
  const connections = useStore((s) => s.connections);
  const toast = useStore((s) => s.toast);
  const [items, setItems] = useState<AiSessionRecord[]>([]);
  const [q, setQ] = useState("");

  const load = () =>
    api
      .aiSessions(null)
      .then(setItems)
      .catch((e) => toast(toError(e).message, "error"));
  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [version]);

  const shown = items.filter((s) => s.title.toLowerCase().includes(q.toLowerCase()));
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="p-2">
        <input className="field py-1" placeholder="Search conversations" aria-label="Search conversations" value={q} onChange={(e) => setQ(e.target.value)} />
      </div>
      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-2">
        {shown.length === 0 && <div className="py-8 text-center text-[12.5px] text-muted">No conversations yet</div>}
        {shown.map((s) => {
          const conn = connections.find((c) => c.id === s.connection_id);
          return (
            <div
              key={s.id}
              role="button"
              tabIndex={0}
              onClick={() => void loadSession(s.id)}
              onKeyDown={(e) => e.key === "Enter" && void loadSession(s.id)}
              className={`group mb-0.5 cursor-pointer rounded-md px-2 py-1.5 hover:bg-hover ${current === s.id ? "bg-hover" : ""}`}
            >
              <div className="flex items-center gap-1.5">
                <span className="min-w-0 flex-1 truncate text-[12.5px]">{s.title}</span>
                <button
                  className="icon-btn hidden h-5 w-5 group-hover:flex"
                  aria-label={`Delete ${s.title}`}
                  onClick={(e) => {
                    e.stopPropagation();
                    void api.aiDeleteSession(s.id).then(load);
                  }}
                >
                  <Trash2 size={11} />
                </button>
              </div>
              <div className="mt-0.5 flex items-center gap-2 text-[10.5px] text-muted">
                {conn && (
                  <span className="flex items-center gap-1">
                    <ConnDot color={conn.color} /> {conn.name}
                  </span>
                )}
                <span>{relativeTime(s.updated_at)}</span>
                {s.model && <span className="truncate">{s.model}</span>}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ------------------------------------------------------------------ knowledge

/** Indexed objects shown per page in Knowledge. */
const OBJECT_PAGE = 200;

function Knowledge() {
  const connections = useStore((s) => s.connections);
  const activeConn = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId)?.connection_id ?? null);
  const toast = useStore((s) => s.toast);
  const progress = useAi((s) => s.knowledgeProgress);
  const version = useAi((s) => s.knowledgeVersion);
  const indexKnowledge = useAi((s) => s.indexKnowledge);
  const cancelIndex = useAi((s) => s.cancelIndex);
  const [connId, setConnId] = useState<string | null>(activeConn ?? connections[0]?.id ?? null);
  const [data, setData] = useState<KnowledgeView | null>(null);
  const [filter, setFilter] = useState("");
  const [objects, setObjects] = useState<{ list: KnObject[]; total: number } | null>(null);
  const [note, setNote] = useState({ target: "", body: "" });
  const [targetState, setTargetState] = useState<TargetState>({ status: "empty" });
  const targetOk = targetState.status === "empty" || targetState.status === "ok";
  const [importing, setImporting] = useState<NotesImportPreview | null>(null);
  const prog = connId ? progress[connId] : undefined;
  const connName = connections.find((c) => c.id === connId)?.name ?? "notes";

  const exportNotes = async () => {
    if (!connId) return;
    const path = await saveDialog({ defaultPath: exportFileName(connName), filters: [{ name: "DataBrain notes", extensions: ["json"] }] });
    if (!path) return;
    try {
      const n = await api.knExportNotes(connId, path);
      toast(`Exported ${n} note${n === 1 ? "" : "s"}. Share the file; others import it in Knowledge.`, "success");
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };
  const importNotes = async () => {
    if (!connId) return;
    const path = await openDialog({ multiple: false, directory: false, filters: [{ name: "DataBrain notes", extensions: ["json"] }] });
    if (typeof path !== "string") return;
    try {
      setImporting(await api.knReadNotesFile(connId, path));
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };

  const load = () => {
    if (!connId) return;
    api
      .knGet(connId)
      .then(setData)
      .catch((e) => toast(toError(e).message, "error"));
  };
  useEffect(load, [connId, version]); // eslint-disable-line react-hooks/exhaustive-deps

  // Indexed objects: one page at a time, filtered in the workspace DB (an
  // index can hold 10k+ tables; never load them all into the view).
  useEffect(() => {
    if (!connId) return setObjects(null);
    let stale = false;
    const t = setTimeout(
      () =>
        void api
          .knObjects(connId, filter, 0, OBJECT_PAGE)
          .then((p) => !stale && setObjects({ list: p.objects, total: p.total }))
          .catch(() => {}),
      filter ? 150 : 0,
    );
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [connId, version, filter]);
  const moreObjects = () => {
    if (!connId || !objects) return;
    void api
      .knObjects(connId, filter, objects.list.length, OBJECT_PAGE)
      .then((p) => setObjects((o) => (o ? { list: [...o.list, ...p.objects], total: p.total } : o)))
      .catch((e) => toast(toError(e).message, "error"));
  };

  const saveNote = async (n: Partial<KnNote>) => {
    if (!connId) return;
    try {
      await api.knSaveNote({
        id: n.id ?? "",
        connection_id: connId,
        target: n.target || null,
        body: n.body ?? "",
        author: n.author ?? "user",
        status: n.status ?? "approved",
        created_at: n.created_at ?? 0,
        replaces: n.replaces ?? null,
      });
      load();
      return true;
    } catch (e) {
      toast(toError(e).message, "error");
      return false;
    }
  };

  const scopeList = connections.find((c) => c.id === connId)?.ai_policy?.index_schemas ?? [];
  const scopeText = scopeList.length === 0 ? "" : scopeList.includes("*") ? "all schemas" : scopeList.join(", ");
  const proposed = data?.notes.filter((n) => n.status === "proposed") ?? [];
  const approved = data?.notes.filter((n) => n.status === "approved") ?? [];

  return (
    <div className="min-h-0 flex-1 space-y-3 overflow-auto p-3 text-[12.5px]">
      <div className="flex items-center gap-2">
        <Database size={13} className="text-muted" />
        <select className="field h-7 py-0" aria-label="Connection" value={connId ?? ""} onChange={(e) => setConnId(e.target.value || null)}>
          {connections.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
        </select>
      </div>

      <div className="rounded-lg border border-line p-2.5">
        <div className="flex flex-wrap items-center gap-2">
          <div className="min-w-[9rem] flex-1">
            <div className="font-medium">Schema index</div>
            <div className="text-[11.5px] text-muted">
              {data?.state
                ? `${formatCount(data.state.objects)} objects · ${data.state.schemas.length} schemas · ${relativeTime(data.state.indexed_at)}`
                : "Not indexed yet. The assistant uses this metadata to find relevant tables."}
            </div>
          </div>
          <div className="flex flex-wrap gap-1.5">
          {prog ? (
            <button className="btn-ghost border border-line py-1" onClick={() => connId && void cancelIndex(connId)} title="Stop indexing; finished schemas are kept">
              <X size={12} /> Cancel
            </button>
          ) : (
            <>
              <button
                className="btn-ghost border border-line py-1"
                disabled={!connId}
                title="Choose catalogs and schemas to index"
                onClick={() => connId && void indexKnowledge(connId, { choose: true })}
              >
                <ListChecks size={12} /> Scope…
              </button>
              <button
                className="btn-ghost border border-line py-1"
                disabled={!connId}
                title={data?.state ? "Re-read schemas whose tables or columns changed since the last run" : "Index tables and columns for the AI"}
                onClick={() => connId && void indexKnowledge(connId)}
              >
                <RefreshCw size={12} /> {data?.state ? "Update" : "Index"}
              </button>
              {data?.state && (
                <button
                  className="btn-ghost border border-line py-1"
                  disabled={!connId}
                  title="Re-read every schema, changed or not"
                  onClick={() => connId && void indexKnowledge(connId, { full: true })}
                >
                  Rebuild
                </button>
              )}
            </>
          )}
          </div>
        </div>
        {scopeText && <div className="mt-1 truncate text-[11px] text-muted" title={scopeText}>Scope: {scopeText}</div>}
        {prog && (
          <div className="mt-2">
            <div className="h-1.5 overflow-hidden rounded bg-panel-2">
              <div className="h-full bg-accent transition-all" style={{ width: `${prog.total ? (prog.done / prog.total) * 100 : 5}%` }} />
            </div>
            <div className="mt-1 flex items-center gap-1.5 truncate text-[11px] text-muted">
              <Loader2 size={11} className="shrink-0 animate-spin" />
              {prog.total ? `${prog.done}/${prog.total} schemas${prog.schema ? ` · ${prog.schema}` : ""}` : prog.schema || "Starting…"}
            </div>
          </div>
        )}
        {data?.state?.error && <div className="mt-1 text-[11.5px] text-danger">{data.state.error}</div>}
        <p className="mt-2 text-[11px] text-muted">
          Only metadata (names, types, comments, keys) is indexed locally. What is sent to the model is controlled per connection
          (Edit connection → AI).
        </p>
      </div>

      {proposed.length > 0 && (
        <div>
          <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-muted">Proposed by AI · review</div>
          {proposed.map((n) => {
            const old = n.replaces ? approved.find((a) => a.id === n.replaces) : undefined;
            return (
            <div key={n.id} className="mb-1.5 rounded-lg border border-warning/40 bg-warning/5 p-2">
              <div className="flex items-center gap-1.5 text-[10.5px] font-medium uppercase text-muted">
                {old ? "Update" : "New"}
                {n.target && <span className="font-mono normal-case">{n.target}</span>}
              </div>
              {old && <div className="whitespace-pre-wrap text-muted line-through decoration-muted/60">{old.body}</div>}
              <div className="whitespace-pre-wrap">{n.body}</div>
              <div className="mt-1.5 flex justify-end gap-1">
                <button className="btn-ghost py-0.5" onClick={() => void api.knDeleteNote(n.id).then(load)}>
                  Discard
                </button>
                <button className="btn-primary py-0.5" onClick={() => void saveNote({ ...n, status: "approved" })}>
                  <Check size={12} /> {old ? "Apply update" : "Approve"}
                </button>
              </div>
            </div>
            );
          })}
        </div>
      )}

      <div>
        <div className="mb-1 flex flex-wrap items-center gap-1">
          <span className="text-[11px] font-semibold uppercase tracking-wide text-muted">Notes & glossary</span>
          <button className="btn-ghost ml-auto py-0.5 text-[11.5px]" disabled={!connId} onClick={() => void importNotes()} title="Import a notes file shared by someone else">
            <FileUp size={12} /> Import…
          </button>
          <button className="btn-ghost py-0.5 text-[11.5px]" disabled={!connId || approved.length === 0} onClick={() => void exportNotes()} title="Save approved notes to a file you can share">
            <FileDown size={12} /> Export…
          </button>
        </div>
        {importing && connId && (
          <ImportNotesDialog
            connectionId={connId}
            preview={importing}
            onClose={() => setImporting(null)}
            onDone={() => {
              setImporting(null);
              load();
            }}
          />
        )}
        <form
          className="mb-2 space-y-1.5"
          onSubmit={(e) => {
            e.preventDefault();
            if (!note.body.trim() || !targetOk) return;
            void saveNote({ target: note.target.trim(), body: note.body.trim() }).then((ok) => ok && setNote({ target: "", body: "" }));
          }}
        >
          {connId && (
            <NoteTargetInput
              connId={connId}
              value={note.target}
              onChange={(target) => setNote((n) => ({ ...n, target }))}
              onState={setTargetState}
            />
          )}
          <textarea
            className="field min-h-[52px]"
            placeholder='Business rule, e.g. "Active customer = status IN (1, 2)"'
            aria-label="Note"
            value={note.body}
            onChange={(e) => setNote({ ...note, body: e.target.value })}
          />
          <div className="flex justify-end">
            <button type="submit" className="btn-primary py-1" disabled={!note.body.trim() || !targetOk} title={targetOk ? undefined : "Fix the tables first"}>
              Add note
            </button>
          </div>
        </form>
        {approved.map((n) => (
          <div key={n.id} className="group mb-1 rounded-md bg-panel-2 px-2 py-1.5">
            <div className="flex items-start gap-1">
              <div className="min-w-0 flex-1">
                {n.target && <div className="font-mono text-[11px] text-muted">{n.target}</div>}
                <div className="whitespace-pre-wrap">{n.body}</div>
              </div>
              <button className="icon-btn hidden h-5 w-5 group-hover:flex" aria-label="Delete note" onClick={() => void api.knDeleteNote(n.id).then(load)}>
                <Trash2 size={11} />
              </button>
            </div>
          </div>
        ))}
      </div>

      {objects && (objects.total > 0 || filter) && (
        <div>
          <div className="mb-1 flex flex-wrap items-center gap-2">
            <span className="text-[11px] font-semibold uppercase tracking-wide text-muted">Indexed objects</span>
            <span className="text-[11px] text-muted">{formatCount(objects.total)}</span>
            <input className="field ml-auto h-6 w-36 min-w-0 py-0 text-[11.5px]" placeholder="Filter" aria-label="Filter objects" value={filter} onChange={(e) => setFilter(e.target.value)} />
          </div>
          <div className="space-y-0.5">
            {objects.list.map((o) => (
              <div key={`${o.schema}.${o.name}`} className="rounded px-1.5 py-1 hover:bg-hover" title={o.columns.map((c) => `${c.name} ${c.data_type}`).join("\n")}>
                <span className="font-mono text-[11.5px]">
                  {o.schema}.{o.name}
                </span>
                <span className="ml-1.5 text-[10.5px] text-muted">
                  {o.kind} · {o.columns.length} cols
                </span>
                {o.comment && <div className="truncate text-[11px] text-muted">{o.comment}</div>}
              </div>
            ))}
            {objects.total === 0 && <div className="text-[11px] text-muted">No match</div>}
            {objects.total > objects.list.length && (
              <button className="btn-ghost py-0.5 text-[11.5px]" onClick={moreObjects}>
                Show {formatCount(Math.min(OBJECT_PAGE, objects.total - objects.list.length))} more of {formatCount(objects.total - objects.list.length)}
              </button>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/** The assistant's connections: the current one (from the editor) plus
 *  any others the user ticks, so one question can span several databases.
 *  A list anchored to the button like the model picker's: above it when
 *  there's no room below (the panel footer), never over the question box. */
function ConnectionPicker({ current }: { current: ConnectionView | null }) {
  const connections = useStore((s) => s.connections);
  const also = useAi((s) => s.alsoConnections);
  const setAlso = useAi((s) => s.setAlsoConnections);
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const [active, setActive] = useState(0);
  const [pos, setPos] = useState<ReturnType<typeof placeComboList> | null>(null);
  const btn = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const usable = connections.filter((c) => c.ai_policy?.ai_enabled !== false);
  const extra = usable.filter((c) => c.id !== current?.id && also.includes(c.id));
  const q = filter.trim().toLowerCase();
  // Current first, then ticked, then the rest.
  const items = usable
    .filter((c) => !q || c.name.toLowerCase().includes(q) || c.config.kind.includes(q))
    .sort((a, b) => {
      const rank = (c: ConnectionView) => (c.id === current?.id ? 0 : also.includes(c.id) ? 1 : 2);
      return rank(a) - rank(b) || a.name.localeCompare(b.name);
    });
  const toggle = (id: string) => {
    if (id === current?.id) return;
    setAlso(also.includes(id) ? also.filter((x) => x !== id) : [...also, id]);
  };
  const close = () => {
    setOpen(false);
    setFilter("");
    setActive(0);
  };

  useLayoutEffect(() => {
    if (!open || !btn.current) return;
    const place = () => {
      const r = btn.current!.getBoundingClientRect();
      const p = placeComboList(r, { width: window.innerWidth, height: window.innerHeight }, 280);
      // Room for the filter box and footer too, but never past the window edge.
      const room = p.top !== undefined ? window.innerHeight - p.top - 4 : r.top - 6;
      setPos({ ...p, maxHeight: Math.max(0, Math.min(340, room)) });
    };
    place();
    window.addEventListener("resize", place);
    window.addEventListener("scroll", place, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [open]);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!box.current?.contains(e.target as Node) && !btn.current?.contains(e.target as Node)) close();
    };
    window.addEventListener("mousedown", onDown);
    return () => window.removeEventListener("mousedown", onDown);
  }, [open]);
  useEffect(() => {
    listRef.current?.children[active]?.scrollIntoView({ block: "nearest" });
  }, [active]);

  return (
    <>
      <button
        ref={btn}
        type="button"
        className={`flex min-w-0 items-center gap-1 rounded px-1 text-[11px] hover:bg-hover hover:text-fg ${open ? "bg-hover text-fg" : "text-muted"}`}
        title={["Connections the assistant uses", current?.name, ...extra.map((c) => c.name)].filter(Boolean).join("\n")}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => (open ? close() : setOpen(true))}
      >
        <ConnDot color={current?.color} connected={current?.connected} />
        <span className="truncate">{current?.name ?? "no connection"}</span>
        {extra.length > 0 && <span className="shrink-0 rounded bg-accent/15 px-1 text-accent">+{extra.length}</span>}
        <ChevronDown size={10} className={`shrink-0 transition-transform ${open ? "rotate-180" : ""}`} />
      </button>
      {open &&
        pos &&
        createPortal(
          <div
            ref={box}
            className="fixed z-[70] flex flex-col rounded-md border border-line bg-panel text-fg shadow-lg"
            style={{ left: pos.left, top: pos.top, bottom: pos.bottom, width: pos.width, maxHeight: pos.maxHeight }}
          >
            <input
              autoFocus
              className="m-1 rounded border border-line bg-bg px-2 py-1 text-[11.5px] outline-none focus:border-accent"
              placeholder="Filter connections…"
              aria-label="Filter connections"
              role="combobox"
              aria-expanded
              aria-controls="ai-conn-list"
              aria-activedescendant={items[active] ? `ai-conn-${items[active].id}` : undefined}
              value={filter}
              onChange={(e) => {
                setFilter(e.target.value);
                setActive(0);
              }}
              onKeyDown={(e) => {
                if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                  e.preventDefault();
                  const n = items.length;
                  if (n) setActive((a) => (e.key === "ArrowDown" ? (a + 1) % n : a <= 0 ? n - 1 : a - 1));
                } else if ((e.key === "Enter" || e.key === " ") && items[active] && (e.key === "Enter" || !filter)) {
                  e.preventDefault();
                  toggle(items[active].id);
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  e.stopPropagation();
                  close();
                  btn.current?.focus();
                }
              }}
            />
            <ul ref={listRef} id="ai-conn-list" role="listbox" aria-multiselectable className="min-h-0 flex-1 overflow-auto py-0.5">
              {items.length === 0 && <li className="px-2 py-1 text-[11.5px] text-muted">{usable.length ? "No match" : "No connection has AI turned on."}</li>}
              {items.map((c, i) => {
                const isCurrent = c.id === current?.id;
                const on = isCurrent || also.includes(c.id);
                return (
                  <li
                    key={c.id}
                    id={`ai-conn-${c.id}`}
                    role="option"
                    aria-selected={on}
                    aria-disabled={isCurrent}
                    title={isCurrent ? "The current connection (from the editor) is always used" : on ? "Click to stop using it" : "Click to let the assistant use it too"}
                    className={`flex items-center gap-2 px-2 py-1 text-[11.5px] ${i === active ? "bg-hover" : ""} ${isCurrent ? "cursor-default" : "cursor-pointer"}`}
                    // mousedown, so the filter box keeps focus
                    onMouseDown={(e) => {
                      e.preventDefault();
                      toggle(c.id);
                    }}
                    onMouseEnter={() => setActive(i)}
                  >
                    <span className={`flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded border ${on ? "border-accent bg-accent text-accent-fg" : "border-line"} ${isCurrent ? "opacity-60" : ""}`}>
                      {on && <Check size={10} />}
                    </span>
                    <ConnDot color={c.color} connected={c.connected} />
                    <span className={`min-w-0 flex-1 truncate ${on ? "text-accent" : ""}`}>{c.name}</span>
                    <span className="shrink-0 text-[10px] text-muted">{isCurrent ? "current" : c.config.kind}</span>
                  </li>
                );
              })}
            </ul>
            <div className="flex items-center gap-2 border-t border-line px-2 py-1 text-[10.5px] text-muted">
              <span className="min-w-0 flex-1 truncate">Ticked ones can be looked up and queried too</span>
              {extra.length > 0 && (
                <button type="button" className="shrink-0 text-accent hover:underline" onMouseDown={(e) => e.preventDefault()} onClick={() => setAlso([])}>
                  Only current
                </button>
              )}
            </div>
          </div>,
          document.body,
        )}
    </>
  );
}
