import { BookOpen, History, MessageSquarePlus, Settings2, Sparkles, X } from "lucide-react";
import { useStore } from "../store";
import { useAi } from "../aiStore";
import { kbd } from "../lib/platform";
import { ChatTabs } from "./ai/ChatTabs";
import { Chat } from "./ai/Chat";
import { Sessions } from "./ai/Sessions";
import { Knowledge } from "./ai/Knowledge";

export { SqlBlock } from "./ai/ChatItems";

export function AiPanel() {
  const view = useAi((s) => s.view);
  const setView = useAi((s) => s.setView);
  const setOpen = useAi((s) => s.setOpen);
  const newChat = useAi((s) => s.newChat);
  const activeChatId = useAi((s) => s.activeChatId);
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
          <button className="icon-btn" title="New chat (opens next to the others)" aria-label="New chat" onClick={() => newChat()}>
            <MessageSquarePlus size={14} />
          </button>
          <button className="icon-btn" title="AI settings" aria-label="AI settings" onClick={() => setSettingsOpen(true)}>
            <Settings2 size={14} />
          </button>
          <button className="icon-btn" title={kbd("Close (⌘L)")} aria-label="Close assistant" onClick={() => setOpen(false)}>
            <X size={14} />
          </button>
        </div>
      </div>
      {view === "chat" && <ChatTabs />}
      {view === "chat" && <Chat key={activeChatId} chatId={activeChatId} />}
      {view === "history" && <Sessions />}
      {view === "knowledge" && <Knowledge />}
    </div>
  );
}
