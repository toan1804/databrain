import { useEffect, useState } from "react";
import { useAi } from "../aiStore";
import { inlineCapable } from "../lib/inlineAi";
import { Plus, Trash2 } from "lucide-react";
import { kbd } from "../lib/platform";
import { BUILTIN_SNIPPETS, snippetPreview, type Snippet } from "../lib/snippets";
import { useStore } from "../store";

/** Settings → Editor: inline AI completion and SQL snippets. */
export function EditorSettings() {
  const inline = useStore((s) => s.inlineAiCompletion);
  const setInline = useStore((s) => s.setInlineAiCompletion);
  const choice = useStore((s) => s.inlineAiProvider);
  const setChoice = useStore((s) => s.setInlineAiProvider);
  const providers = useAi((s) => s.providers);
  const panelId = useAi((s) => s.providerId);
  const loadModels = useAi((s) => s.loadModels);
  const capable = inlineCapable(providers);
  // A chosen provider that was removed: the AI panel's is used.
  const chosen = choice && capable.some((p) => p.id === choice.provider_id) ? choice : null;
  const panel = providers.find((p) => p.id === panelId) ?? providers[0];
  const [models, setModels] = useState<string[]>([]);
  useEffect(() => {
    setModels([]);
    if (!chosen) return;
    let alive = true;
    loadModels(chosen.provider_id)
      .then((m) => alive && setModels(m))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [chosen?.provider_id, loadModels]); // eslint-disable-line react-hooks/exhaustive-deps
  const chosenView = chosen ? capable.find((p) => p.id === chosen.provider_id) : undefined;
  const defaultModel = chosenView?.config.fast_model || chosenView?.config.default_model || "";
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
        After a short pause, the provider below suggests the rest of the statement in grey at the cursor. {kbd("Tab")} accepts it,{" "}
        {kbd("⌘→")} accepts the next word, {kbd("Esc")} dismisses it, and {kbd("⌥\\")} asks right away (also with this turned off). The SQL around the
        cursor and a few relevant tables from the connection's knowledge index are sent to the provider; connections with AI turned off, or providers
        the connection doesn't allow, get no suggestions.
      </p>
      <div className="grid grid-cols-[auto_1fr] items-center gap-x-2 gap-y-1.5 text-[12px]">
        <label htmlFor="inline-provider" className="text-muted">
          Provider
        </label>
        <select
          id="inline-provider"
          className="field h-7 py-0"
          value={chosen?.provider_id ?? ""}
          onChange={(e) => setChoice(e.target.value ? { provider_id: e.target.value, model: "" } : null)}
        >
          <option value="">Same as the AI panel{panel ? ` (${panel.name})` : ""}</option>
          {capable.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        {chosen && (
          <>
            <label htmlFor="inline-model" className="text-muted">
              Model
            </label>
            <input
              id="inline-model"
              className="field h-7 py-0 font-mono text-[11.5px]"
              list="inline-models"
              placeholder={defaultModel ? `${defaultModel} (the provider's ${chosenView?.config.fast_model ? "fast" : "default"} model)` : "Model"}
              value={chosen.model}
              onChange={(e) => setChoice({ ...chosen, model: e.target.value })}
            />
            <datalist id="inline-models">
              {models.map((m) => (
                <option key={m} value={m} />
              ))}
            </datalist>
          </>
        )}
      </div>
      {!chosen && panel?.kind === "kiro" && (
        <p className="text-[11.5px] text-warning">
          The AI panel uses Kiro, which runs as an agent and can't complete inline: choose another provider here (a local model in Ollama or LM Studio
          works well and keeps the SQL on this computer).
        </p>
      )}
      {capable.length === 0 && providers.length > 0 && (
        <p className="text-[11.5px] text-muted">Add a model provider (OpenAI, Anthropic, Gemini, Ollama, …) in Settings → AI providers to get suggestions.</p>
      )}

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
