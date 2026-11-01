import { useEffect, useMemo, useRef, useState } from "react";
import { useStickToBottom } from "../../lib/hooks";
import { Bot, Clock, FileCode2, Lightbulb, Loader2, Send, ShieldAlert, Sparkles, Square, Workflow, Wrench, X } from "lucide-react";
import type { AiMode } from "../../lib/types";
import { useStore } from "../../store";
import { connectionForKey, useAi, visibleItems, workingStatus, type ChatItem } from "../../aiStore";
import { extractMentions, mentionToken, resultsConnection } from "../../outputs";
import { ComboInput, ListPicker } from "../ui";
import { UsageFooter } from "../ProviderUsage";
import { drafts } from "./drafts";
import { ChatItemView } from "./ChatItems";
import { AutoApproveToggle, ConnectionPicker } from "./pickers";
import { INPUT_H_DEFAULT, INPUT_H_KEY, InputResizeHandle, clampInputH } from "./InputResizeHandle";

const NO_MODELS: string[] = [];

const QUICK: { mode: AiMode; label: string; prompt: string; icon: React.ReactNode }[] = [
  { mode: "generate", label: "Write SQL", prompt: "", icon: <FileCode2 size={12} /> },
  { mode: "explain", label: "Explain query", prompt: "Explain what the query in my editor does.", icon: <Lightbulb size={12} /> },
  { mode: "fix_error", label: "Fix error", prompt: "Fix the error in my query.", icon: <Wrench size={12} /> },
  { mode: "analyze_result", label: "Analyze result", prompt: "Analyze the current result and summarize the key findings.", icon: <Sparkles size={12} /> },
];

export function Chat({ chatId }: { chatId: string }) {
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
  // Only what the chat uses of the active tab (not the tab: it changes as its text is saved).
  const activeTabId = useStore((s) => s.activeTabId);
  const activeJobId = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId)?.job_id ?? null);
  const activeTitle = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId)?.title ?? "");
  const activeConnId = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId)?.connection_id ?? null);
  const activeTab = activeTabId ? { id: activeTabId, job_id: activeJobId, title: activeTitle, connection_id: activeConnId } : undefined;
  const setSettingsOpen = useStore((s) => s.setSettingsOpen);
  const [text, setText] = useState(() => drafts.get(chatId) ?? "");
  useEffect(() => {
    if (text) drafts.set(chatId, text);
    else drafts.delete(chatId);
  }, [chatId, text]);
  const [mode, setMode] = useState<AiMode>("chat");
  const models = useAi((s) => (providerId ? s.models[providerId] : undefined)) ?? NO_MODELS;
  const outputs = useStore((s) => s.outputs);
  const [mention, setMention] = useState<{ start: number; query: string; index: number } | null>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLTextAreaElement>(null);
  // Height of the question box (drag its top edge), kept across restarts.
  const [inputH, setInputH] = useState(() => clampInputH(Number(localStorage.getItem(INPUT_H_KEY)) || INPUT_H_DEFAULT));

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

  // Follows the answer while the user is at the bottom; scrolling up to read stops it.
  const stick = useStickToBottom(scroller, items);

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
    stick.pin();
    void send({ chatId, message: msg.trim(), mode: m, connectionId: connId, targetKey: jobTab ? null : undefined, context: Object.keys(context).length ? context : undefined });
    setText("");
    setMode("chat");
  };

  const aiDisabled = conn && conn.ai_policy && !conn.ai_policy.ai_enabled;

  return (
    <>
      <div ref={scroller} onScroll={stick.onScroll} className="min-h-0 flex-1 space-y-3 overflow-auto p-3">
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
        {running && <WorkingBar items={items} onStop={() => void cancel(chatId)} />}
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
          <InputResizeHandle height={inputH} onChange={setInputH} />
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
            style={{ height: inputH }}
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
            <AutoApproveToggle />
            {jobTab && (
              <span className="flex min-w-0 shrink items-center gap-1 rounded bg-accent/10 px-1 text-[11px] text-accent" title="The assistant knows which job is open">
                <Workflow size={10} className="shrink-0" />
                <span className="truncate">{jobTab.name}</span>
              </span>
            )}
            <ListPicker
              className="ml-auto h-6 w-[110px] rounded border border-line bg-panel px-1 text-[11px] outline-none hover:border-accent/60 focus:border-accent"
              aria-label="AI provider"
              placeholder="No provider"
              value={providerId}
              options={providers.map((p) => ({ value: p.id, label: p.name, hint: p.kind }))}
              onChange={(v) => setProvider(v || null)}
            />
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
