import { codeFolding, foldGutter, foldKeymap, foldService } from "@codemirror/language";
import type { Extension, Text } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";

/** A foldable section: its opener starts at `open`, the folded text is `from..to`. */
export interface FoldPair {
  open: number;
  from: number;
  to: number;
}

const WORD = /[A-Za-z0-9_$#]/;
/** `END IF`, `END LOOP` …: closes a block that never opened one here. */
const END_OTHER = new Set(["IF", "LOOP", "WHILE", "REPEAT", "FOR"]);

/**
 * Sections of a SQL text (UTF-16 offsets): parentheses (subqueries, column
 * lists, CTE bodies), `BEGIN … END` and `CASE … END` blocks, and
 * `-- region` / `-- endregion` comments. Strings, quoted identifiers and
 * comments are skipped. Only sections spanning several lines are kept.
 */
export function sqlFoldPairs(doc: string): FoldPair[] {
  const out: FoldPair[] = [];
  const stack: { kind: "(" | "BEGIN" | "CASE"; at: number; after: number }[] = [];
  const regions: { at: number; after: number }[] = [];
  const lineEnd = (i: number) => {
    const n = doc.indexOf("\n", i);
    return n < 0 ? doc.length : n;
  };
  const push = (open: number, from: number, to: number) => {
    // Trailing whitespace before the closer stays visible.
    while (to > from && /\s/.test(doc[to - 1])) to--;
    if (to > from && doc.slice(from, to).includes("\n")) out.push({ open, from, to });
  };
  let i = 0;
  while (i < doc.length) {
    const c = doc[i];
    const next = doc[i + 1];
    if (c === "-" && next === "-") {
      const end = lineEnd(i);
      const text = doc.slice(i + 2, end).trim().toLowerCase();
      if (/^#?region\b/.test(text)) regions.push({ at: i, after: end });
      else if (/^#?endregion\b/.test(text)) {
        const r = regions.pop();
        if (r) push(r.at, r.after, i);
      }
      i = end;
      continue;
    }
    if (c === "/" && next === "*") {
      const end = doc.indexOf("*/", i + 2);
      i = end < 0 ? doc.length : end + 2;
      continue;
    }
    if (c === "'" || c === '"' || c === "`") {
      let j = i + 1;
      while (j < doc.length) {
        if (doc[j] === c) {
          if (doc[j + 1] === c) j += 2; // doubled quote
          else break;
        } else j++;
      }
      i = j + 1;
      continue;
    }
    if (c === "(") {
      stack.push({ kind: "(", at: i, after: i + 1 });
      i++;
      continue;
    }
    if (c === ")") {
      // Unbalanced keywords inside parentheses are dropped with them.
      while (stack.length && stack[stack.length - 1].kind !== "(") stack.pop();
      const o = stack.pop();
      if (o) push(o.at, o.after, i);
      i++;
      continue;
    }
    if (WORD.test(c) && (i === 0 || !WORD.test(doc[i - 1]))) {
      let j = i;
      while (j < doc.length && WORD.test(doc[j])) j++;
      const w = doc.slice(i, j).toUpperCase();
      if (w === "BEGIN" || w === "CASE") {
        // BEGIN TRANSACTION / BEGIN TRAN / BEGIN; start no block.
        const rest = doc.slice(j, j + 40).toUpperCase();
        if (w === "CASE" || !/^\s*(TRANSACTION|TRAN|WORK|DEFERRED|IMMEDIATE|EXCLUSIVE|DISTRIBUTED)?\s*(;|$)/.test(rest)) {
          stack.push({ kind: w, at: i, after: w === "BEGIN" ? lineEnd(j) : j });
        }
      } else if (w === "END") {
        const after = doc.slice(j).match(/^\s+([A-Za-z_]+)/)?.[1]?.toUpperCase();
        if (!after || !END_OTHER.has(after)) {
          let top = stack.length - 1;
          if (after === "CASE") while (top >= 0 && stack[top].kind !== "CASE") top--;
          if (top >= 0 && stack[top].kind !== "(") {
            const o = stack[top];
            stack.length = top;
            push(o.at, o.after, i);
          }
        }
      }
      i = j;
      continue;
    }
    i++;
  }
  return out.sort((a, b) => a.open - b.open);
}

// One scan per document version.
const pairsCache = new WeakMap<Text, FoldPair[]>();
function pairsOf(doc: Text): FoldPair[] {
  let p = pairsCache.get(doc);
  if (!p) {
    p = sqlFoldPairs(doc.toString());
    pairsCache.set(doc, p);
  }
  return p;
}

/**
 * Collapsible sections in a SQL editor: a gutter with ▾/▸ markers, ⌥⌘[ /
 * ⌥⌘] (fold / unfold), ⌃⌥[ / ⌃⌥] (all). Besides the statements and block
 * comments the SQL grammar folds, parentheses, BEGIN/CASE … END and
 * `-- region` comments fold from the line they open on.
 */
export function sqlFolding(): Extension {
  return [
    foldService.of((state, lineStart, lineEnd) => {
      // Outermost section opening on this line that ends on a later line.
      for (const p of pairsOf(state.doc)) {
        if (p.open > lineEnd) break;
        if (p.open >= lineStart && p.to > lineEnd) return { from: Math.max(p.from, p.open + 1), to: p.to };
      }
      return null;
    }),
    codeFolding({ placeholderText: "…" }),
    foldGutter({
      markerDOM: (open) => {
        const el = document.createElement("span");
        el.className = "cm-fold-marker";
        el.textContent = open ? "▾" : "▸";
        el.title = open ? "Collapse" : "Expand";
        return el;
      },
    }),
    keymap.of(foldKeymap),
    EditorView.theme({
      ".cm-foldGutter .cm-gutterElement": { cursor: "pointer", padding: "0 3px", color: "var(--muted, #888)" },
      ".cm-fold-marker": { opacity: "0.55" },
      ".cm-foldGutter .cm-gutterElement:hover .cm-fold-marker": { opacity: "1" },
      ".cm-foldPlaceholder": {
        background: "var(--panel-2, transparent)",
        border: "1px solid var(--border, #8884)",
        borderRadius: "4px",
        padding: "0 5px",
        margin: "0 2px",
        color: "var(--muted, #888)",
        cursor: "pointer",
      },
    }),
  ];
}
