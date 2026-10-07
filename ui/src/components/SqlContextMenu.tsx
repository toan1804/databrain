import { useEffect, useState } from "react";
import type { EditorView } from "@codemirror/view";
import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";
import { ClipboardCopy, ClipboardPaste, Network, Scissors, TextSelect, WandSparkles } from "lucide-react";
import { toError } from "../lib/api";
import type { ConnectorKind } from "../lib/types";
import { useStore } from "../store";
import { MenuItem, MenuSeparator, Popover } from "./ui";

/**
 * Format the selection (or the whole script when nothing is selected) in
 * one undoable change; the formatted text stays selected. `false` when it
 * could not be parsed (a toast says why).
 */
export async function formatInView(view: EditorView, kind: ConnectorKind | undefined, whole = false): Promise<boolean> {
  const sel = view.state.selection.main;
  const [from, to] = whole || sel.empty ? [0, view.state.doc.length] : [sel.from, sel.to];
  const text = view.state.sliceDoc(from, to);
  if (!text.trim()) return false;
  let out: string;
  try {
    // Loaded on first use (the formatter's dialect grammars are large).
    const { formatSql } = await import("../lib/formatSql");
    out = formatSql(text, kind);
  } catch (e) {
    useStore.getState().toast(`Could not format: ${toError(e).message}`, "error");
    return false;
  }
  // Edited meanwhile: don't overwrite.
  if (view.state.sliceDoc(from, to) !== text) return false;
  if (out !== text) {
    view.dispatch({
      changes: { from, to, insert: out },
      selection: whole || sel.empty ? undefined : { anchor: from, head: from + out.length },
      scrollIntoView: true,
      userEvent: "input.format",
    });
  }
  view.focus();
  return true;
}

/** Shift-Alt-F: format selection / script (same as the context menu). */
export const formatKey = (getView: () => EditorView | null, getKind: () => ConnectorKind | undefined) => ({
  key: "Shift-Alt-f",
  run: () => {
    const v = getView();
    return v ? (void formatInView(v, getKind()), true) : false;
  },
  preventDefault: true,
});

async function copy(text: string) {
  try {
    await writeText(text);
  } catch {
    await navigator.clipboard?.writeText(text);
  }
}

async function paste(): Promise<string> {
  try {
    return (await readText()) ?? "";
  } catch {
    return (await navigator.clipboard?.readText?.()) ?? "";
  }
}

/** Right-click menu of a SQL editor: clipboard, select all, format SQL. */
export function SqlContextMenu({
  view,
  kind,
  at,
  onClose,
  onLineage,
}: {
  view: EditorView;
  kind: ConnectorKind | undefined;
  at: { x: number; y: number };
  onClose: () => void;
  /** "Show lineage" (selection or whole script) in a new tab. */
  onLineage?: () => void;
}) {
  const [sel] = useState(() => view.state.selection.main);
  const hasSel = !sel.empty;
  const readOnly = view.state.readOnly;
  const run = (f: () => void | Promise<void>) => () => {
    onClose();
    void f();
  };
  // Closing returns focus to the editor (keyboard users).
  useEffect(() => () => view.focus(), [view]);
  const mod = navigator.platform.includes("Mac") ? "⌘" : "Ctrl+";
  return (
    <Popover x={at.x} y={at.y} onClose={onClose} className="w-60">
      <div role="menu" aria-label="Editor actions">
        <MenuItem
          icon={<WandSparkles size={13} />}
          label={hasSel ? "Format selection" : "Format SQL"}
          hint="⇧⌥F"
          disabled={readOnly}
          onClick={run(() => void formatInView(view, kind))}
        />
        {hasSel && <MenuItem icon={<WandSparkles size={13} />} label="Format whole script" disabled={readOnly} onClick={run(() => void formatInView(view, kind, true))} />}
        {onLineage && <MenuItem icon={<Network size={13} />} label={hasSel ? "Show lineage of selection" : "Show lineage"} hint="new tab" onClick={run(onLineage)} />}
        <MenuSeparator />
        <MenuItem
          icon={<Scissors size={13} />}
          label="Cut"
          hint={`${mod}X`}
          disabled={!hasSel || readOnly}
          onClick={run(async () => {
            await copy(view.state.sliceDoc(sel.from, sel.to));
            view.dispatch({ changes: { from: sel.from, to: sel.to }, userEvent: "delete.cut" });
          })}
        />
        <MenuItem icon={<ClipboardCopy size={13} />} label="Copy" hint={`${mod}C`} disabled={!hasSel} onClick={run(() => copy(view.state.sliceDoc(sel.from, sel.to)))} />
        <MenuItem
          icon={<ClipboardPaste size={13} />}
          label="Paste"
          hint={`${mod}V`}
          disabled={readOnly}
          onClick={run(async () => {
            const text = await paste();
            if (text) view.dispatch({ changes: { from: sel.from, to: sel.to, insert: text }, selection: { anchor: sel.from + text.length }, userEvent: "input.paste" });
          })}
        />
        <MenuItem icon={<TextSelect size={13} />} label="Select all" hint={`${mod}A`} onClick={run(() => view.dispatch({ selection: { anchor: 0, head: view.state.doc.length } }))} />
      </div>
    </Popover>
  );
}
