// Notes & glossary sharing: turn an import preview into actions, and build
// the request that lets the assistant merge conflicting notes.
import type { ImportAction, ImportItem } from "./types";

/** Per-conflict choice in the import dialog. */
export type ConflictChoice = "keep" | "theirs" | "both" | "merge" | "ai";

export interface ImportChoices {
  /** Index into items → add this new note. */
  addNew: Record<number, boolean>;
  conflicts: Record<number, ConflictChoice>;
  /** Edited merged text for "merge". */
  merged: Record<number, string>;
}

/** Defaults: add every new note; conflicts keep the current note. */
export function defaultChoices(items: ImportItem[]): ImportChoices {
  const c: ImportChoices = { addNew: {}, conflicts: {}, merged: {} };
  items.forEach((it, i) => {
    if (it.kind === "new") c.addNew[i] = true;
    if (it.kind === "conflict") {
      c.conflicts[i] = "keep";
      c.merged[i] = mergeTexts(it.existing.map((e) => e.body), it.incoming.body);
    }
  });
  return c;
}

/** Simple text merge: existing lines, then incoming lines not already present. */
export function mergeTexts(existing: string[], incoming: string): string {
  const seen = new Set<string>();
  const out: string[] = [];
  const add = (line: string) => {
    const k = line.trim().toLowerCase().replace(/\s+/g, " ");
    if (!k || seen.has(k)) return;
    seen.add(k);
    out.push(line.trim());
  };
  for (const e of existing) e.split("\n").forEach(add);
  incoming.split("\n").forEach(add);
  return out.join("\n");
}

/** Actions applied directly (everything except "ai"). */
export function toActions(items: ImportItem[], c: ImportChoices): ImportAction[] {
  const acts: ImportAction[] = [];
  items.forEach((it, i) => {
    if (it.kind === "new" && c.addNew[i]) acts.push({ incoming: it.incoming, action: "add", existing_ids: [] });
    if (it.kind !== "conflict") return;
    const ids = it.existing.map((e) => e.id);
    switch (c.conflicts[i]) {
      case "theirs":
        acts.push({ incoming: it.incoming, action: "replace", existing_ids: ids });
        break;
      case "both":
        acts.push({ incoming: it.incoming, action: "add", existing_ids: [] });
        break;
      case "merge":
        acts.push({ incoming: it.incoming, action: "merge", existing_ids: ids, body: c.merged[i] ?? it.incoming.body, target: it.existing[0]?.target ?? it.incoming.target });
        break;
    }
  });
  return acts;
}

/** Conflicts the user handed to the assistant. */
export function aiConflicts(items: ImportItem[], c: ImportChoices): ImportItem[] {
  return items.filter((it, i) => it.kind === "conflict" && c.conflicts[i] === "ai");
}

const label = (n: { target?: string | null }) => n.target || "glossary";

/**
 * Chat message asking the assistant to merge imported notes into existing
 * ones with update_knowledge_note (each merge comes back for review unless
 * the connection saves AI notes directly).
 */
export function aiMergePrompt(items: ImportItem[], source?: string): string {
  const lines = [
    `Merge these imported knowledge notes${source ? ` (from "${source}")` : ""} into this connection's notes & glossary.`,
    "For each one, update the existing note with update_knowledge_note so that it keeps every fact from both versions in one clear note. " +
      "If the two versions contradict each other, keep both statements and mark the contradiction. Do not run queries. " +
      "Finish with a one-line summary per note.",
    "",
  ];
  items.forEach((it, i) => {
    lines.push(`${i + 1}. ${label(it.incoming)}`);
    for (const e of it.existing) lines.push(`   existing (id ${e.id}): ${e.body.replace(/\n/g, " ")}`);
    lines.push(`   imported: ${it.incoming.body.replace(/\n/g, " ")}`);
  });
  return lines.join("\n");
}

/** File name for an export, e.g. `shop-db.databrain-notes.json`. */
export function exportFileName(connectionName: string): string {
  const base = connectionName.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "notes";
  return `${base}.databrain-notes.json`;
}

/** Summary line of an import preview. */
export function previewSummary(items: ImportItem[]): { new: number; same: number; conflict: number } {
  return {
    new: items.filter((i) => i.kind === "new").length,
    same: items.filter((i) => i.kind === "same").length,
    conflict: items.filter((i) => i.kind === "conflict").length,
  };
}

