// Column matching for "Compare outputs" (same rules as the backend's
// `column_pairs`): user pairs first, then equal names, then names equal
// ignoring case. Each column is used once.

export interface ColumnPair {
  before: string;
  after: string;
  how: "manual" | "same" | "case";
}

/**
 * `excluded`: after-side columns the user left out (not matched
 * automatically).
 */
export function pairColumns(before: string[], after: string[], manual: [string, string][], excluded: string[] = []): { pairs: ColumnPair[]; onlyBefore: string[]; onlyAfter: string[] } {
  const usedB = new Set<string>();
  const usedA = new Set<string>(excluded.filter((e) => !manual.some(([, a]) => a === e)));
  const pairs: ColumnPair[] = [];
  for (const [b, a] of manual) {
    if (!before.includes(b) || !after.includes(a) || usedB.has(b) || usedA.has(a)) continue;
    usedB.add(b);
    usedA.add(a);
    pairs.push({ before: b, after: a, how: "manual" });
  }
  for (const exact of [true, false]) {
    for (const a of after) {
      if (usedA.has(a)) continue;
      const b = before.find((x) => !usedB.has(x) && (exact ? x === a : x.toLowerCase() === a.toLowerCase()));
      if (b === undefined) continue;
      usedB.add(b);
      usedA.add(a);
      pairs.push({ before: b, after: a, how: exact ? "same" : "case" });
    }
  }
  pairs.sort((x, y) => after.indexOf(x.after) - after.indexOf(y.after));
  const paired = new Set(pairs.map((p) => p.after));
  return { pairs, onlyBefore: before.filter((b) => !usedB.has(b)), onlyAfter: after.filter((a) => !paired.has(a)) };
}

/** Set (or clear, with `b` = null) the before column of after column `a`; a before column used elsewhere moves here. */
export function setPair(manual: [string, string][], excluded: string[], a: string, b: string | null): { manual: [string, string][]; excluded: string[] } {
  const m = manual.filter(([x, y]) => y !== a && x !== b);
  const ex = excluded.filter((e) => e !== a);
  return b === null ? { manual: m, excluded: [...ex, a] } : { manual: [...m, [b, a]], excluded: ex };
}

/**
 * Pairs typed as `a = a1, b = b2` (also `->`, `→`, `↔`, `:`; commas,
 * semicolons or new lines between pairs). `r1.a = r2.a1` works too: a
 * prefix before the first dot is dropped when the whole name isn't a column.
 */
export function parseMapping(text: string, before: string[], after: string[]): { pairs: [string, string][]; errors: string[] } {
  const pick = (name: string, cols: string[]): string | undefined => {
    const t = name.trim().replace(/^["`[]|["`\]]$/g, "");
    const find = (n: string) => cols.find((c) => c === n) ?? cols.find((c) => c.toLowerCase() === n.toLowerCase());
    return find(t) ?? (t.includes(".") ? find(t.slice(t.indexOf(".") + 1)) : undefined);
  };
  const pairs: [string, string][] = [];
  const errors: string[] = [];
  for (const part of text.split(/[,;\n]+/).map((p) => p.trim()).filter(Boolean)) {
    const m = part.split(/\s*(?:<->|->|=>|↔|→|=|:)\s*/);
    if (m.length !== 2 || !m[0] || !m[1]) {
      errors.push(`"${part}": write it as before = after`);
      continue;
    }
    const b = pick(m[0], before);
    const a = pick(m[1], after);
    if (!b) errors.push(`${m[0]} is not a column of the before output`);
    else if (!a) errors.push(`${m[1]} is not a column of the after output`);
    else pairs.push([b, a]);
  }
  return { pairs, errors };
}
