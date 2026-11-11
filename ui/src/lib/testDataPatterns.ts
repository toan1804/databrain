// Column patterns in Generate test data: a rule a column's values follow
// (`18..90`, `%@gmail.com`, `INV-####`). A column's default comes from its
// name; the user's edits and the AI's suggestions are remembered per table
// (setting `test_data_patterns`).
import type { TestDataColumn } from "./types";

/** How to write patterns (as the backend reads them). */
export const PATTERN_HELP = [
  "Numbers: lo..hi (18..90, 0.5..99.99)",
  "Dates: 1950-01-01..2005-12-31 or 1950..2005; times 08:00..18:00",
  "Any column: {a|b|c}, one of these values",
  "Text: # a digit, ? a letter, % a word that suits the column (a name, a city, first.last in an email column), %d %m %y %Y %H %M %S parts of a date, {a|b} one of, \\ keeps the next character",
  "Examples: %@gmail.com, INV-#####, +84 ### ### ###, %d/%m/%Y",
].join("\n");

/** Where a table's patterns are remembered. */
export function patternsKey(connectionId: string, schema: string, table: string): string {
  return `${connectionId}|${schema}.${table}`;
}

export function parseStoredPatterns(v: unknown): Record<string, Record<string, string>> {
  if (!v || typeof v !== "object" || Array.isArray(v)) return {};
  const out: Record<string, Record<string, string>> = {};
  for (const [k, m] of Object.entries(v as Record<string, unknown>)) {
    if (!m || typeof m !== "object" || Array.isArray(m)) continue;
    const cols = Object.entries(m as Record<string, unknown>).filter((e): e is [string, string] => typeof e[1] === "string");
    if (cols.length) out[k] = Object.fromEntries(cols);
  }
  return out;
}

/** A column's pattern: the user's (or the AI's), else its default. */
export function patternOf(c: TestDataColumn, overrides: Record<string, string>): string {
  return overrides[c.name] ?? c.pattern ?? "";
}

/** The patterns sent: every column that takes one (`""`: none). */
export function patternsFor(cols: TestDataColumn[], overrides: Record<string, string>): Record<string, string> {
  return Object.fromEntries(cols.filter((c) => c.pattern_hint).map((c) => [c.name, patternOf(c, overrides).trim()]));
}
