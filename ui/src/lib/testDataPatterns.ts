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

export interface GuideExample {
  pattern: string;
  /** What it makes, e.g. "linh.tran@gmail.com". */
  gives: string;
}

export interface GuideSection {
  /** The column types it is for. */
  title: string;
  /** How to write it. */
  how: string;
  examples: GuideExample[];
}

/** The Pattern guide in the dialog: the syntax by column type, with examples to click. */
export const PATTERN_GUIDE: GuideSection[] = [
  {
    title: "Numbers",
    how: "lo..hi: a value from lo to hi.",
    examples: [
      { pattern: "18..90", gives: "an age: 18 to 90" },
      { pattern: "1..5", gives: "a rating: 1 to 5" },
      { pattern: "0.5..99.99", gives: "a price with decimals" },
    ],
  },
  {
    title: "Dates and date-times",
    how: "first..last, as dates (YYYY-MM-DD) or years. A date column stores dates: formats such as %d/%m/%Y are for text columns.",
    examples: [
      { pattern: "1950-01-01..2005-12-31", gives: "a date of birth" },
      { pattern: "2024..2025", gives: "any day in 2024 or 2025" },
    ],
  },
  { title: "Times", how: "HH:MM..HH:MM.", examples: [{ pattern: "08:00..18:00", gives: "office hours" }] },
  {
    title: "Any column",
    how: "{a|b|c}: one of these values.",
    examples: [{ pattern: "{gold|silver|bronze}", gives: "silver" }],
  },
  {
    title: "Text",
    how: "A template: # a digit, ? a letter (A–Z), % a word that suits the column (a name, a city; first.last in an email column), %d %m %y %Y %H %M %S parts of a date, {a|b} one of. Anything else stays as written; \\ keeps the next character (\\# is a #).",
    examples: [
      { pattern: "%@gmail.com", gives: "linh.tran@gmail.com" },
      { pattern: "INV-#####", gives: "INV-04817" },
      { pattern: "+84 ### ### ###", gives: "+84 912 345 678" },
      { pattern: "??-####", gives: "QX-2041" },
      { pattern: "%d/%m/%Y", gives: "07/03/2024" },
      { pattern: "{Mr.|Ms.} %", gives: "Ms. Nora Kim" },
    ],
  },
];

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
