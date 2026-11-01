import type { KeyboardEvent } from "react";

/** How far a row is indented: its own padding plus its wrappers' (folder contents are wrapped). */
function indent(row: HTMLElement, tree: Element): number {
  let n = 0;
  for (let el: HTMLElement | null = row; el && el !== tree; el = el.parentElement) n += parseFloat(el.style.paddingLeft) || 0;
  return n;
}

/**
 * Arrow-key navigation of a `role="tree"` whose rows are `role="treeitem"`
 * (WAI-ARIA tree pattern): ↑/↓ previous/next row, Home/End first/last,
 * → expands a closed row or moves into an open one, ← collapses an open row
 * or moves to its parent. Rows that aren't rendered (inside closed nodes)
 * are skipped by construction. Returns whether the key was handled.
 */
export function treeKeyDown(e: KeyboardEvent<HTMLElement>, row: { expanded?: boolean; toggle?: () => void }): boolean {
  if (e.altKey || e.metaKey || e.ctrlKey) return false;
  const self = e.currentTarget;
  const tree = self.closest('[role="tree"]');
  if (!tree || e.target !== self) return false;
  const rows = Array.from(tree.querySelectorAll<HTMLElement>('[role="treeitem"]'));
  const i = rows.indexOf(self);
  const go = (el: HTMLElement | undefined) => {
    if (!el) return;
    el.focus();
    el.scrollIntoView({ block: "nearest" });
  };
  switch (e.key) {
    case "ArrowDown":
      go(rows[i + 1]);
      break;
    case "ArrowUp":
      go(rows[i - 1]);
      break;
    case "Home":
      go(rows[0]);
      break;
    case "End":
      go(rows[rows.length - 1]);
      break;
    case "ArrowRight":
      if (row.expanded === false) row.toggle?.();
      else if (row.expanded === true && rows[i + 1] && indent(rows[i + 1], tree) > indent(self, tree)) go(rows[i + 1]);
      break;
    case "ArrowLeft": {
      if (row.expanded === true) {
        row.toggle?.();
        break;
      }
      const mine = indent(self, tree);
      for (let j = i - 1; j >= 0; j--) {
        if (indent(rows[j], tree) < mine) {
          go(rows[j]);
          break;
        }
      }
      break;
    }
    default:
      return false;
  }
  e.preventDefault();
  return true;
}
