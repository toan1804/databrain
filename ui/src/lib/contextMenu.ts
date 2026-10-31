// Right-click shows DataBrain's own menus only. Where the app has none, the
// web view would open its default menu (Reload, Back, Inspect Element…),
// which doesn't belong in a desktop app. Text fields keep it: there it is
// the usual Cut / Copy / Paste.

const TEXT_TYPES = new Set(["", "text", "search", "url", "email", "password", "number", "tel"]);

/** Whether a right-click on `target` may open the system menu (a text field). */
export function allowsSystemMenu(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false;
  const field = target.closest("input, textarea");
  if (field instanceof HTMLTextAreaElement) return !field.disabled;
  if (field instanceof HTMLInputElement) return !field.disabled && TEXT_TYPES.has((field.getAttribute("type") ?? "").toLowerCase());
  return false;
}

/** Block the system menu everywhere but text fields; the app's own menus still open. */
export function installAppContextMenu(doc: Document = document): () => void {
  const onMenu = (e: MouseEvent) => {
    if (!allowsSystemMenu(e.target)) e.preventDefault();
  };
  doc.addEventListener("contextmenu", onMenu);
  return () => doc.removeEventListener("contextmenu", onMenu);
}
