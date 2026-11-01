// Pure rules for inline AI ("ghost") completion: when to ask, what text to
// send, and how to fit the model's answer at the cursor.

const PREFIX_CHARS = 4000;
const SUFFIX_CHARS = 1500;
const MAX_LINES = 6;

/** The text sent around the cursor. */
export function ghostWindow(doc: string, pos: number): { prefix: string; suffix: string } {
  return { prefix: doc.slice(Math.max(0, pos - PREFIX_CHARS), pos), suffix: doc.slice(pos, pos + SUFFIX_CHARS) };
}

/**
 * Whether a suggestion makes sense at `pos`: something was written, the rest
 * of the line is empty (or closing brackets/quotes), the cursor is not in a
 * `--` comment or a string, and the statement isn't finished (`;`).
 */
export function canSuggest(doc: string, pos: number): boolean {
  const lineStart = doc.lastIndexOf("\n", pos - 1) + 1;
  const lineEndIdx = doc.indexOf("\n", pos);
  const after = doc.slice(pos, lineEndIdx < 0 ? doc.length : lineEndIdx);
  if (!/^[\s)\]'"`;,]*$/.test(after)) return false;
  const before = doc.slice(lineStart, pos);
  if (/--/.test(before.replace(/'[^']*'/g, ""))) return false;
  if ((before.match(/'/g)?.length ?? 0) % 2 === 1) return false;
  const recent = doc.slice(Math.max(0, pos - 400), pos);
  const last = recent.trimEnd().slice(-1);
  if (!last || last === ";") return false;
  // At least a word of SQL.
  return /[A-Za-z_]{2}/.test(recent);
}

/** The model's answer as it goes in at the cursor (`""` = nothing to show). */
export function fitGhost(text: string, prefix: string, suffix: string): string {
  let t = text.replace(/\r\n/g, "\n");
  const line = prefix.slice(prefix.lastIndexOf("\n") + 1);
  // The model repeated the current line.
  if (line.trim() && t.startsWith(line)) t = t.slice(line.length);
  else if (line.trim() && t.trimStart().startsWith(line.trim()) && line.trim().length > 3) t = t.trimStart().slice(line.trim().length);
  // No double space where the line already ends with one.
  if (/\s$/.test(prefix) && !prefix.endsWith("\n")) t = t.replace(/^[ \t]+/, "");
  // A new line at the start of an empty line.
  if ((prefix === "" || prefix.endsWith("\n")) && t.startsWith("\n")) t = t.replace(/^\n+/, "");
  // What already follows the cursor on this line (`)`, `'`) is not repeated.
  const rest = suffix.slice(0, suffix.indexOf("\n") < 0 ? suffix.length : suffix.indexOf("\n")).trim();
  for (let k = Math.min(rest.length, t.length); k > 0; k--) {
    if (t.trimEnd().endsWith(rest.slice(0, k))) {
      t = t.trimEnd().slice(0, -k);
      break;
    }
  }
  const lines = t.split("\n");
  if (lines.length > MAX_LINES) t = lines.slice(0, MAX_LINES).join("\n");
  t = t.replace(/\s+$/, "");
  return t.trim() ? t : "";
}

/** What is left of a suggestion after `typed` was typed at its start (`null`: it no longer fits). */
export function advanceGhost(ghost: string, typed: string): string | null {
  if (!typed || !ghost.startsWith(typed)) return null;
  const rest = ghost.slice(typed.length);
  return rest.trim() ? rest : null;
}

/** The suggestion up to the end of its next word (⌘→ accepts word by word). */
export function nextWord(ghost: string): string {
  const m = /^\s*[^\s]+/.exec(ghost);
  return m ? m[0] : ghost;
}
