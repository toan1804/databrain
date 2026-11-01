/** A field the user types into: ⌘↵ there belongs to the field, not to the active tab. */
const TYPING = "input, textarea, select, [contenteditable]:not([contenteditable='false']), [role='textbox'], [role='combobox']";

/**
 * Should the global ⌘↵ / Ctrl+↵ run the active tab's SQL for this key event?
 * Not from a SQL editor (it runs its own), not from a text field such as a
 * dialog's input, the AI prompt or a grid cell editor, and not when a
 * component already handled the key. A field can opt in with
 * `data-run-shortcut` (the tab's parameter bar).
 */
export function globalRunApplies(e: Pick<KeyboardEvent, "target" | "defaultPrevented">): boolean {
  if (e.defaultPrevented) return false;
  const t = e.target instanceof Element ? e.target : null;
  if (!t) return true;
  if (t.closest(".cm-editor")) return false;
  if (t.closest("[data-run-shortcut]")) return true;
  return !t.closest(TYPING);
}
