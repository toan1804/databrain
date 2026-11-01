import { useEffect, useRef } from "react";
import { Loader2, MessageSquarePlus, ShieldAlert, X } from "lucide-react";
import { useStore } from "../../store";
import { conversationTitle, useAi, type Conversation } from "../../aiStore";
import { drafts } from "./drafts";

/** Open conversations as tabs; each keeps working while another is shown. */
export function ChatTabs() {
  const chats = useAi((s) => s.chats);
  const active = useAi((s) => s.activeChatId);
  const switchChat = useAi((s) => s.switchChat);
  const closeChat = useAi((s) => s.closeChat);
  const newChat = useAi((s) => s.newChat);
  const bar = useRef<HTMLDivElement>(null);

  // Keep the shown conversation in view.
  useEffect(() => {
    const el = bar.current;
    const tab = el?.querySelector<HTMLElement>('[aria-selected="true"]');
    if (!el || !tab) return;
    const b = el.getBoundingClientRect();
    const r = tab.getBoundingClientRect();
    if (r.left < b.left) el.scrollLeft -= b.left - r.left + 4;
    else if (r.right > b.right) el.scrollLeft += r.right - b.right + 4;
  }, [active, chats.length]);

  const close = (c: Conversation) => {
    const pending = c.items.some((i) => (i.kind === "approval" || i.kind === "edit") && i.state === "pending");
    if (!c.running && !pending) {
      drafts.delete(c.id);
      return closeChat(c.id);
    }
    useStore.getState().askConfirm({
      title: `Close "${conversationTitle(c)}"?`,
      reasons: ["The assistant is still working on it: closing stops it and declines what waits for approval. The conversation so far stays in History."],
      confirmLabel: "Stop and close",
      onConfirm: () => {
        drafts.delete(c.id);
        closeChat(c.id);
      },
    });
  };

  // One blank conversation: nothing to switch between.
  if (chats.length === 1 && !chats[0].sessionId && chats[0].items.length === 0) return null;

  return (
    <div
      ref={bar}
      role="tablist"
      aria-label="Conversations"
      className="flex h-8 shrink-0 items-center gap-0.5 overflow-x-auto border-b border-line px-1.5"
      onWheel={(e) => {
        if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) e.currentTarget.scrollLeft += e.deltaY;
      }}
    >
      {chats.map((c) => {
        const selected = c.id === active;
        const title = conversationTitle(c);
        const waiting = c.items.some((i) => (i.kind === "approval" || i.kind === "edit") && i.state === "pending");
        return (
          <div
            key={c.id}
            role="tab"
            tabIndex={0}
            aria-selected={selected}
            title={`${title}${c.running ? (waiting ? " · waiting for your approval" : " · working…") : c.unread ? " · new answer" : ""}`}
            onClick={() => switchChat(c.id)}
            onAuxClick={(e) => e.button === 1 && close(c)}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                switchChat(c.id);
              }
            }}
            className={`group flex h-6 min-w-[72px] max-w-[160px] shrink-0 cursor-pointer items-center gap-1 rounded-md px-1.5 text-[11.5px] ${
              selected ? "bg-hover text-fg" : "text-muted hover:bg-hover/60 hover:text-fg"
            }`}
          >
            {waiting ? (
              <ShieldAlert size={11} className="shrink-0 text-warning" aria-label="Waiting for approval" />
            ) : c.running || c.loading ? (
              <Loader2 size={11} className="shrink-0 animate-spin text-accent" aria-label={c.loading ? "Loading" : "Working"} />
            ) : c.unread ? (
              <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-accent" aria-label="New answer" />
            ) : null}
            <span className="min-w-0 flex-1 truncate">{title}</span>
            <button
              aria-label={`Close ${title}`}
              title="Close (stays in History)"
              onClick={(e) => {
                e.stopPropagation();
                close(c);
              }}
              className={`shrink-0 rounded p-0.5 text-muted hover:bg-panel hover:text-fg ${selected ? "" : "opacity-0 group-hover:opacity-100 focus:opacity-100"}`}
            >
              <X size={10} />
            </button>
          </div>
        );
      })}
      <button className="icon-btn h-6 w-6 shrink-0" title="New chat" aria-label="New chat" onClick={() => newChat()}>
        <MessageSquarePlus size={12} />
      </button>
    </div>
  );
}
