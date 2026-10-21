import { useState } from "react";
import { Loader2, Sparkles } from "lucide-react";
import { useAi } from "../aiStore";
import { useStore } from "../store";
import { Markdown } from "./Markdown";

export { connectionErrorQuestion } from "../lib/aiQuestions";

/** The connection the conversation is filed under (the assistant doesn't use it): this one if it is saved with AI on, else the AI panel's, else any with AI on. */
function aiConnection(preferred: string | null): string | null {
  const conns = useStore.getState().connections;
  const ok = (id: string | null | undefined) => !!id && conns.some((c) => c.id === id && c.ai_policy?.ai_enabled);
  if (ok(preferred)) return preferred;
  const panel = useAi.getState().connectionId;
  if (ok(panel)) return panel;
  return conns.find((c) => c.ai_policy?.ai_enabled)?.id ?? null;
}

/**
 * "Ask AI" for an error shown in a dialog: the answer is shown right there
 * (dialogs cover the AI panel), and the conversation stays in AI history.
 */
export function AskAiAboutError({ error, question, connectionId }: { error: string; question: () => string; connectionId: string | null }) {
  // Index in the AI conversation where our question starts.
  const [since, setSince] = useState<number | null>(null);
  const [asked, setAsked] = useState<string | null>(null);
  const items = useAi((s) => s.items);
  const running = useAi((s) => s.running);
  const toast = useStore((s) => s.toast);

  const ask = async () => {
    const ai = useAi.getState();
    if (ai.running) return toast("The assistant is busy with another question; try again when it finishes", "info");
    if (!ai.providerId) {
      toast("Add an AI provider first", "info");
      return useStore.getState().setSettingsOpen(true);
    }
    const conn = aiConnection(connectionId);
    if (!conn) return toast("Turn on AI for one connection first (its AI settings); the assistant runs on a connection", "error");
    ai.newChat();
    setSince(0);
    setAsked(error);
    // The question is about the settings, not data: the assistant must not
    // connect to the connection it runs on (that could start its sign-in).
    await ai.send({
      message: question(),
      connectionId: conn,
      targetKey: null,
      context: { no_database: true, editor_sql: null, selection: null, last_error: null, failed_sql: null, failed_line: null, result_id: null, connections: [], mentions: [] },
    });
  };

  const answer = since === null || asked !== error ? [] : items.slice(since).filter((i) => i.kind === "assistant" || i.kind === "error");
  const last = answer.at(-1);
  const thinking = since !== null && asked === error && running && !(last?.kind === "assistant" && last.text);

  return (
    <>
      <button type="button" className="btn-ghost -my-1 h-6 shrink-0 px-1.5 py-0 text-[11.5px]" title="Ask the AI assistant what this error means and how to fix it" onClick={() => void ask()} disabled={thinking}>
        {thinking ? <Loader2 size={12} className="animate-spin" /> : <Sparkles size={12} />} Ask AI
      </button>
      {since !== null && asked === error && (thinking || last) && (
        <div className="basis-full rounded-md border border-line bg-panel p-2 text-[12.5px]" aria-live="polite">
          {last?.kind === "assistant" && last.text ? (
            <Markdown text={last.text} className="text-[12.5px]" />
          ) : last?.kind === "error" ? (
            <span className="text-danger">{last.text}</span>
          ) : (
            <span className="text-muted">Asking the assistant…</span>
          )}
          {!running && last?.kind === "assistant" && <p className="mt-1.5 text-[11px] text-muted">The conversation is also in the AI panel (⌘L), to ask more.</p>}
        </div>
      )}
    </>
  );
}
