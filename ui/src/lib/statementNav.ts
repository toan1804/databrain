/** Next / previous statement navigation in the SQL editor. */

export interface StatementRange {
  start: number;
  end: number;
}

/**
 * Where the cursor goes: the start of the statement after the one at
 * `cursor` (`dir` 1), or of the one before it (`dir` -1). From inside a
 * statement (not at its start), "previous" goes to that statement's start.
 * `null` at either end.
 */
export function statementTarget(spans: StatementRange[], cursor: number, dir: 1 | -1): number | null {
  if (!spans.length) return null;
  if (dir === 1) {
    const next = spans.find((s) => s.start > cursor);
    return next ? next.start : null;
  }
  const before = spans.filter((s) => s.start < cursor);
  return before.length ? before[before.length - 1].start : null;
}
