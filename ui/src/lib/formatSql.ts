// SQL formatting for the editor (selection or whole script), per dialect.
import { format, type SqlLanguage } from "sql-formatter";
import type { ConnectorKind } from "./types";
import { traitsOf } from "./sourceTraits";

export function formatterDialect(kind: ConnectorKind | undefined): SqlLanguage {
  return (traitsOf(kind).formatter || "sql") as SqlLanguage;
}

/** Where a parameter (`:name`, Oracle `&name`) is in the text (UTF-16 offsets). */
export interface Span {
  from: number;
  to: number;
}

/**
 * `sql` with each parameter replaced by a placeholder word the formatter
 * reads as a plain name, and how to put them back. The formatter's own
 * parameter rules differ by dialect: most refuse `:name`, and some split it
 * into `: name`, which no longer is a parameter.
 */
export function protectParams(sql: string, params: Span[]): { text: string; restore: (s: string) => string } {
  const spans = params.filter((p) => p.from >= 0 && p.to <= sql.length && p.from < p.to).sort((a, b) => a.from - b.from);
  if (!spans.length) return { text: sql, restore: (s) => s };
  let tag = "dbparam";
  while (sql.includes(tag)) tag += "x";
  const originals: string[] = [];
  let text = "";
  let at = 0;
  for (const p of spans) {
    if (p.from < at) continue; // overlapping: keep the first
    text += sql.slice(at, p.from) + `${tag}${originals.length}${tag}`;
    originals.push(sql.slice(p.from, p.to));
    at = p.to;
  }
  text += sql.slice(at);
  const re = new RegExp(`${tag}(\\d+)${tag}`, "g");
  return { text, restore: (s) => s.replace(re, (_, i: string) => originals[Number(i)]) };
}

/**
 * Format `sql` for a connection kind. Keeps the leading indentation and
 * trailing whitespace of the original (so a formatted selection sits where it
 * was). Falls back to the generic dialect when the engine's parser rejects it;
 * throws when neither can parse it. `params` are the parameters in `sql`
 * (from the backend's `sql_parameters`): kept exactly as written.
 */
export function formatSql(sql: string, kind: ConnectorKind | undefined, opts: { tabWidth?: number; uppercase?: boolean; params?: Span[] } = {}): string {
  const lead = sql.match(/^\s*/)?.[0] ?? "";
  const trail = sql.match(/\s*$/)?.[0] ?? "";
  const body0 = sql.trim();
  if (!body0) return sql;
  // Parameters' offsets in the trimmed body.
  const { text: body, restore } = protectParams(
    body0,
    (opts.params ?? []).map((p) => ({ from: p.from - lead.length, to: p.to - lead.length })),
  );
  if (traitsOf(kind).language !== "sql") throw new Error(`Format works on SQL; ${traitsOf(kind).name} queries are left as written`);
  // Indent continuation lines like the first line (selection inside a block).
  const indent = lead.slice(lead.lastIndexOf("\n") + 1);
  const run = (language: SqlLanguage) =>
    format(body, {
      language,
      tabWidth: opts.tabWidth ?? 2,
      keywordCase: opts.uppercase === false ? "preserve" : "upper",
      linesBetweenQueries: 1,
    });
  let out: string;
  try {
    out = run(formatterDialect(kind));
  } catch (e) {
    if (formatterDialect(kind) === "sql") throw e;
    out = run("sql");
  }
  out = restore(out);
  if (indent) out = out.split("\n").map((l, i) => (i === 0 || !l ? l : indent + l)).join("\n");
  return lead + out + trail;
}
