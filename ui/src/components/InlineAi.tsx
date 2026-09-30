import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Sparkles, X } from "lucide-react";
import { useAi } from "../aiStore";
import { editorBridge } from "../editorBridge";

/**
 * ⌘I prompt anchored at the cursor of the editor that fired `db:inline-ai`.
 * With a selection the AI rewrites it; otherwise it generates SQL at the
 * cursor. The proposal shows as a diff in the assistant panel to accept.
 */
export function InlineAi() {
  const [state, setState] = useState<{ key: string; x: number; y: number; hasSelection: boolean } | null>(null);
  const [text, setText] = useState("");
  const send = useAi((s) => s.send);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const onOpen = (e: Event) => {
      const key = (e as CustomEvent<{ key: string }>).detail.key;
      const v = editorBridge.get(key);
      if (!v) return;
      const sel = v.state.selection.main;
      const c = v.coordsAtPos(sel.from) ?? v.dom.getBoundingClientRect();
      const rect = v.dom.getBoundingClientRect();
      setState({ key, x: Math.max(rect.left + 8, Math.min(c.left, window.innerWidth - 480)), y: Math.max(8, c.top - 44), hasSelection: sel.to > sel.from });
      setText("");
      setTimeout(() => input.current?.focus(), 0);
    };
    window.addEventListener("db:inline-ai", onOpen);
    return () => window.removeEventListener("db:inline-ai", onOpen);
  }, []);

  if (!state) return null;
  const close = () => {
    editorBridge.focus(state.key);
    setState(null);
  };
  const submit = () => {
    if (!text.trim()) return;
    const doc = editorBridge.snapshot(state.key)?.sql ?? "";
    void send({ message: text.trim(), mode: state.hasSelection || doc.trim() ? "edit" : "generate", targetKey: state.key });
    setState(null);
  };
  return createPortal(
    <div className="fixed z-50 w-[460px] rounded-lg border border-accent/60 bg-panel p-1.5 shadow-2xl" style={{ left: state.x, top: state.y }}>
      <form
        className="flex items-center gap-1.5"
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <Sparkles size={14} className="ml-1 shrink-0 text-accent" />
        <input
          ref={input}
          className="h-7 min-w-0 flex-1 bg-transparent text-[13px] outline-none"
          placeholder={state.hasSelection ? "Edit selection… e.g. add a filter for last 30 days" : "Generate SQL… e.g. top 10 customers by revenue"}
          aria-label="AI instruction"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => e.key === "Escape" && close()}
          onBlur={() => setTimeout(() => setState((s) => (s && document.activeElement !== input.current ? null : s)), 150)}
        />
        <span className="kbd">↵</span>
        <button type="button" className="icon-btn h-6 w-6" aria-label="Close" onClick={close}>
          <X size={13} />
        </button>
      </form>
    </div>,
    document.body,
  );
}
