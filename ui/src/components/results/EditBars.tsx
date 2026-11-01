import type { Dispatch, SetStateAction } from "react";
import { Loader2, Pencil } from "lucide-react";
import type { ConnectionView, EditStatement, EditTarget } from "../../lib/types";
import { emptyEdits, type PendingEdits } from "../../lib/gridEdits";
import { formatCount } from "../../lib/util";
import { Modal } from "../ui";

/** Shown while Edit checks the table (schema, columns, primary key). */
export function CheckingEditBanner({ conn }: { conn: ConnectionView | undefined }) {
  return (
    <div className="flex shrink-0 items-start gap-2 border-b border-line bg-accent/8 px-2 py-1.5 text-[12px]" role="status" aria-live="polite">
      <Loader2 size={13} className="mt-0.5 shrink-0 animate-spin text-accent" />
      <div className="min-w-0">
        <div>Checking {conn?.name ?? "the database"} before editing…</div>
        <div className="text-[11.5px] text-muted">The table this result comes from, its schema, columns and primary key (from the explorer's cache when it has them).</div>
      </div>
    </div>
  );
}

/** Editing the result's table: pending change count, preview, discard, save, done. */
export function EditingBar({
  target,
  changes,
  saving,
  setEdits,
  showPreview,
  saveEdits,
  stopEditing,
}: {
  target: EditTarget;
  changes: number;
  saving: boolean;
  setEdits: Dispatch<SetStateAction<PendingEdits>>;
  showPreview: () => Promise<void>;
  saveEdits: () => Promise<void>;
  stopEditing: () => void;
}) {
  return (
    <div className="flex h-9 shrink-0 items-center gap-2 border-b border-line bg-accent/8 px-2 text-[12px]">
      <Pencil size={13} className="shrink-0 text-accent" />
      <span className="min-w-0 shrink truncate">
        Editing <span className="font-mono">{target.schema ? `${target.schema}.` : ""}{target.table}</span>
      </span>
      <span className="min-w-0 flex-1 truncate text-muted" aria-live="polite">
        · {changes ? `${formatCount(changes)} change${changes === 1 ? "" : "s"}` : "double-click a cell to change it, right-click for rows"}
      </span>
      {target.apply === "statements" && (
        <span className="shrink-0 whitespace-nowrap rounded bg-warning/15 px-1.5 text-warning" title="This database has no transactions here: changes are written one statement at a time">
          no rollback
        </span>
      )}
      <button className="btn-ghost h-6 shrink-0 whitespace-nowrap py-0 text-[11.5px]" disabled={!changes} onClick={() => void showPreview()}>
        Preview SQL
      </button>
      <button className="btn-ghost h-6 shrink-0 whitespace-nowrap py-0 text-[11.5px]" disabled={!changes} onClick={() => setEdits(emptyEdits())}>
        Discard
      </button>
      <button className="btn-primary h-6 shrink-0 whitespace-nowrap py-0 text-[11.5px]" disabled={!changes || saving} onClick={() => void saveEdits()}>
        {saving ? <Loader2 size={12} className="animate-spin" /> : null} Save
      </button>
      <button className="btn-ghost h-6 shrink-0 whitespace-nowrap py-0 text-[11.5px]" onClick={stopEditing}>
        Done editing
      </button>
    </div>
  );
}

/** The SQL that saving the pending edits runs. */
export function EditPreviewModal({
  preview,
  setPreview,
  target,
  saving,
  saveEdits,
}: {
  preview: EditStatement[];
  setPreview: Dispatch<SetStateAction<EditStatement[] | null>>;
  target: EditTarget | null;
  saving: boolean;
  saveEdits: () => Promise<void>;
}) {
  return (
    <Modal
      title="SQL to save"
      width={720}
      onClose={() => setPreview(null)}
      footer={
        <>
          <button className="btn-ghost" onClick={() => void navigator.clipboard.writeText(preview.map((s) => `${s.sql};`).join("\n"))}>
            Copy
          </button>
          <button className="btn-ghost" onClick={() => setPreview(null)}>
            Close
          </button>
          <button className="btn-primary" disabled={saving} onClick={() => void saveEdits()}>
            Save
          </button>
        </>
      }
    >
      <p className="mb-2 text-[12px] text-muted">
        {target?.apply === "statements"
          ? "Run one by one: this database has no transaction to roll back."
          : "Run in one transaction; each UPDATE and DELETE must change exactly one row or everything is rolled back."}
      </p>
      <pre className="max-h-[50vh] overflow-auto rounded-md border border-line bg-panel-2 p-2 font-mono text-[11.5px]">
        {preview.map((s) => `${s.sql};`).join("\n")}
      </pre>
    </Modal>
  );
}
