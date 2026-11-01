import { useState } from "react";
import { Plus, Trash2 } from "lucide-react";
import { kbd } from "../lib/platform";
import { BUILTIN_SNIPPETS, snippetPreview, type Snippet } from "../lib/snippets";
import { useStore } from "../store";

/** Settings → Editor: inline AI completion and SQL snippets. */
export function EditorSettings() {
  const inline = useStore((s) => s.inlineAiCompletion);
  const setInline = useStore((s) => s.setInlineAiCompletion);
  const snippets = useStore((s) => s.sqlSnippets);
  const setSnippets = useStore((s) => s.setSqlSnippets);
  const [draft, setDraft] = useState<Snippet>({ prefix: "", body: "", description: "" });
  const prefixOk = /^[\w$#@-]+$/.test(draft.prefix.trim());
  const canAdd = prefixOk && draft.body.trim() !== "";
  const add = () => {
    if (!canAdd) return;
    const p = draft.prefix.trim();
    setSnippets([...snippets.filter((s) => s.prefix.toLowerCase() !== p.toLowerCase()), { prefix: p, body: draft.body, description: draft.description?.trim() || undefined }]);
    setDraft({ prefix: "", body: "", description: "" });
  };
  return (
    <div className="max-w-xl space-y-2 text-[12.5px]">
      <h3 className="text-[13px] font-medium">Inline AI completion</h3>
      <label className="flex items-center gap-2 text-[12px]">
        <input type="checkbox" checked={inline} onChange={(e) => setInline(e.target.checked)} /> Suggest how the SQL goes on while I type
      </label>
      <p className="text-[11.5px] text-muted">
        After a short pause, the AI panel's provider (its fast model when set) suggests the rest of the statement in grey at the cursor. {kbd("Tab")} accepts it,{" "}
        {kbd("⌘→")} accepts the next word, {kbd("Esc")} dismisses it, and {kbd("⌥\\")} asks right away (also with this turned off). The SQL around the
        cursor and a few relevant tables from the connection's knowledge index are sent to the provider; connections with AI turned off, or providers
        the connection doesn't allow, get no suggestions. Kiro can't be used for this.
      </p>

      <h3 className="pt-3 text-[13px] font-medium">SQL snippets</h3>
      <p className="text-[11.5px] text-muted">
        Type a snippet's prefix in the editor and pick it from the completion list. <code>${"{name}"}</code> marks a field: {kbd("Tab")} goes to the next one,{" "}
        {kbd("⇧Tab")} back. Your snippets come first; one with the prefix of a built-in snippet replaces it.
      </p>
      {snippets.length > 0 && (
        <ul className="divide-y divide-line rounded-md border border-line">
          {snippets.map((s) => (
            <li key={s.prefix} className="flex items-start gap-2 px-2 py-1.5">
              <code className="w-20 shrink-0 truncate font-medium">{s.prefix}</code>
              <div className="min-w-0 flex-1">
                {s.description && <div className="text-[11.5px] text-muted">{s.description}</div>}
                <pre className="whitespace-pre-wrap break-words font-mono text-[11.5px]">{snippetPreview(s.body)}</pre>
              </div>
              <button
                className="icon-btn h-6 w-6 shrink-0"
                aria-label={`Delete snippet ${s.prefix}`}
                title="Delete"
                onClick={() => setSnippets(snippets.filter((x) => x.prefix !== s.prefix))}
              >
                <Trash2 size={12} />
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="space-y-1.5 rounded-md border border-line p-2">
        <div className="flex gap-2">
          <input
            className={`field w-28 ${draft.prefix && !prefixOk ? "border-danger" : ""}`}
            placeholder="Prefix"
            aria-label="Snippet prefix"
            value={draft.prefix}
            onChange={(e) => setDraft({ ...draft, prefix: e.target.value })}
          />
          <input className="field min-w-0 flex-1" placeholder="Description (optional)" aria-label="Snippet description" value={draft.description ?? ""} onChange={(e) => setDraft({ ...draft, description: e.target.value })} />
        </div>
        <textarea
          className="field h-20 w-full font-mono text-[12px]"
          placeholder={"SELECT ${columns}\nFROM ${table}"}
          aria-label="Snippet text"
          value={draft.body}
          onChange={(e) => setDraft({ ...draft, body: e.target.value })}
        />
        <button className="btn-ghost border border-line" disabled={!canAdd} onClick={add}>
          <Plus size={13} /> Add snippet
        </button>
      </div>
      <details className="text-[12px]">
        <summary className="cursor-pointer text-muted">Built-in snippets ({BUILTIN_SNIPPETS.length})</summary>
        <ul className="mt-1 space-y-1">
          {BUILTIN_SNIPPETS.map((s) => (
            <li key={s.prefix} className="flex gap-2">
              <code className="w-12 shrink-0 font-medium">{s.prefix}</code>
              <span className="text-muted">{s.description}</span>
            </li>
          ))}
        </ul>
      </details>
    </div>
  );
}
