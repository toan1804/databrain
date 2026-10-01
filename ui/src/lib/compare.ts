// Column matching for "Compare outputs" (same rules as the backend's
// `column_pairs`): user pairs first, then equal names, then names equal
// ignoring case. Each column is used once.

export interface ColumnPair {
  before: string;
  after: string;
  how: "manual" | "same" | "case";
}

export function pairColumns(before: string[], after: string[], manual: [string, string][]): { pairs: ColumnPair[]; onlyBefore: string[]; onlyAfter: string[] } {
  const usedB = new Set<string>();
  const usedA = new Set<string>();
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
  return { pairs, onlyBefore: before.filter((b) => !usedB.has(b)), onlyAfter: after.filter((a) => !usedA.has(a)) };
}
