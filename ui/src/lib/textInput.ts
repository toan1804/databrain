// The macOS web view applies the system's text substitutions (capitalize
// words, autocorrect, smart quotes) to editable fields. SQL, identifiers and
// filters must stay exactly as typed, so every text field opts out.

const TEXT_TYPES = new Set(["", "text", "search", "url", "email", "password", "number", "tel"]);

/** Turn off auto-capitalization/correction on a text field (idempotent). */
export function plainTextField(el: Element | null): void {
  if (!el) return;
  const isInput = el instanceof HTMLInputElement && TEXT_TYPES.has((el.getAttribute("type") ?? "").toLowerCase());
  const isEditable = el instanceof HTMLTextAreaElement || (el instanceof HTMLElement && el.isContentEditable);
  if (!isInput && !isEditable) return;
  // Respect fields that explicitly opt in (none today).
  if (el.getAttribute("data-autotext") === "on") return;
  el.setAttribute("autocapitalize", "off");
  el.setAttribute("autocorrect", "off");
  el.setAttribute("autocomplete", el.getAttribute("autocomplete") ?? "off");
  el.setAttribute("spellcheck", "false");
}

/** Apply to the focused field before the first keystroke reaches it. */
export function installPlainTextFields(doc: Document = document): () => void {
  const onFocus = (e: FocusEvent) => plainTextField(e.target as Element | null);
  doc.addEventListener("focusin", onFocus, true);
  return () => doc.removeEventListener("focusin", onFocus, true);
}
