import { useState } from "react";
import { FileUp, Sparkles } from "lucide-react";
import { api, toError } from "../lib/api";
import { aiConflicts, aiMergePrompt, defaultChoices, previewSummary, toActions, type ConflictChoice, type ImportChoices } from "../lib/notesImport";
import type { NotesImportPreview } from "../lib/types";
import { useAi } from "../aiStore";
import { useStore } from "../store";
import { Modal } from "./ui";

const CHOICES: { value: ConflictChoice; label: string }[] = [
  { value: "keep", label: "Keep mine" },
  { value: "theirs", label: "Use the file's" },
  { value: "both", label: "Keep both" },
  { value: "merge", label: "Merge (edit)" },
  { value: "ai", label: "Merge with AI" },
];

/** Review an imported notes file: add new notes, resolve conflicts. */
export function ImportNotesDialog({
  connectionId,
  preview,
  onClose,
  onDone,
}: {
  connectionId: string;
  preview: NotesImportPreview;
  onClose: () => void;
  onDone: () => void;
}) {
  const items = preview.items;
  const [c, setC] = useState<ImportChoices>(() => defaultChoices(items));
  const [busy, setBusy] = useState(false);
  const sum = previewSummary(items);
  const toast = useStore((s) => s.toast);
  const forAi = aiConflicts(items, c);
  const setAll = (v: ConflictChoice) =>
    setC((x) => ({ ...x, conflicts: Object.fromEntries(Object.keys(x.conflicts).map((k) => [k, v])) }));

  const apply = async () => {
    setBusy(true);
    try {
      const actions = toActions(items, c);
      const n = actions.length ? await api.knImportNotes(connectionId, actions) : 0;
      if (forAi.length) {
        // The assistant proposes merged notes; they show up for review in Knowledge.
        const ai = useAi.getState();
        ai.setOpen(true, "chat");
        await ai.send({ message: aiMergePrompt(forAi, preview.source?.connection), connectionId });
      }
      toast(
        `Imported ${n} note${n === 1 ? "" : "s"}${forAi.length ? `; the assistant is merging ${forAi.length} more (review them in Knowledge)` : ""}`,
        "success",
      );
      onDone();
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <FileUp size={15} /> Import notes{preview.source ? ` from "${preview.source.connection}"` : ""}
        </span>
      }
      onClose={onClose}
      width={640}
      footer={
        <>
          <span className="mr-auto text-[11.5px] text-muted">
            {sum.new} new · {sum.conflict} different · {sum.same} already here
          </span>
          <button className="btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button className="btn-primary" disabled={busy || (toActions(items, c).length === 0 && forAi.length === 0)} onClick={() => void apply()}>
            {forAi.length ? <Sparkles size={13} /> : null} Import
          </button>
        </>
      }
    >
      <div className="max-h-[60vh] space-y-3 overflow-auto text-[12.5px]">
        {sum.new > 0 && (
          <section>
            <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-muted">New ({sum.new})</div>
            {items.map(
              (it, i) =>
                it.kind === "new" && (
                  <label key={i} className="flex cursor-pointer items-start gap-2 rounded px-1 py-0.5 hover:bg-hover">
                    <input type="checkbox" className="mt-1" checked={!!c.addNew[i]} onChange={(e) => setC({ ...c, addNew: { ...c.addNew, [i]: e.target.checked } })} />
                    <span className="min-w-0">
                      {it.incoming.target && <span className="mr-1.5 font-mono text-[11px] text-muted">{it.incoming.target}</span>}
                      <span className="whitespace-pre-wrap">{it.incoming.body}</span>
                    </span>
                  </label>
                ),
            )}
          </section>
        )}
        {sum.conflict > 0 && (
          <section>
            <div className="mb-1 flex items-center gap-2">
              <span className="text-[11px] font-semibold uppercase tracking-wide text-muted">Different from yours ({sum.conflict})</span>
              <span className="ml-auto text-[11px] text-muted">All:</span>
              {CHOICES.filter((x) => x.value !== "merge").map((x) => (
                <button key={x.value} className="text-[11px] text-accent hover:underline" onClick={() => setAll(x.value)}>
                  {x.label}
                </button>
              ))}
            </div>
            {items.map(
              (it, i) =>
                it.kind === "conflict" && (
                  <div key={i} className="mb-2 rounded-md border border-line p-2">
                    <div className="mb-1 font-mono text-[11px] text-muted">{it.incoming.target || it.existing[0]?.target || "glossary"}</div>
                    <div className="grid grid-cols-2 gap-2">
                      <div>
                        <div className="text-[10.5px] font-medium uppercase text-muted">Yours</div>
                        {it.existing.map((e) => (
                          <div key={e.id} className="whitespace-pre-wrap">
                            {e.body}
                          </div>
                        ))}
                      </div>
                      <div>
                        <div className="text-[10.5px] font-medium uppercase text-muted">File</div>
                        <div className="whitespace-pre-wrap">{it.incoming.body}</div>
                      </div>
                    </div>
                    <div className="mt-1.5 flex flex-wrap gap-x-3 gap-y-1" role="radiogroup" aria-label="How to import this note">
                      {CHOICES.map((x) => (
                        <label key={x.value} className="flex cursor-pointer items-center gap-1 text-[12px]">
                          <input type="radio" name={`c${i}`} checked={c.conflicts[i] === x.value} onChange={() => setC({ ...c, conflicts: { ...c.conflicts, [i]: x.value } })} />
                          {x.value === "ai" && <Sparkles size={11} className="text-accent" />}
                          {x.label}
                        </label>
                      ))}
                    </div>
                    {c.conflicts[i] === "merge" && (
                      <textarea
                        className="field mt-1.5 min-h-[60px]"
                        aria-label="Merged note"
                        value={c.merged[i] ?? ""}
                        onChange={(e) => setC({ ...c, merged: { ...c.merged, [i]: e.target.value } })}
                      />
                    )}
                  </div>
                ),
            )}
            {forAi.length > 0 && (
              <p className="text-[11.5px] text-muted">
                "Merge with AI" opens the assistant, which combines each pair into one note. Its merges wait for your review in Knowledge (unless this
                connection saves AI notes directly). Note text is sent to the AI provider.
              </p>
            )}
          </section>
        )}
        {sum.new === 0 && sum.conflict === 0 && <p className="text-muted">Every note in this file is already here.</p>}
      </div>
    </Modal>
  );
}
