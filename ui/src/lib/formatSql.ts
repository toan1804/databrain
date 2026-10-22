// SQL formatting for the editor (selection or whole script), per dialect.
import { format, type SqlLanguage } from "sql-formatter";
import type { ConnectorKind } from "./types";
import { traitsOf } from "./sourceTraits";

export function formatterDialect(kind: ConnectorKind | undefined): SqlLanguage {
  return (traitsOf(kind).formatter || "sql") as SqlLanguage;
}

/**
 * Format `sql` for a connection kind. Keeps the leading indentation and
 * trailing whitespace of the original (so a formatted selection sits where it
 * was). Falls back to the generic dialect when the engine's parser rejects it;
 * throws when neither can parse it.
 */
export function formatSql(sql: string, kind: ConnectorKind | undefined, opts: { tabWidth?: number; uppercase?: boolean } = {}): string {
  const lead = sql.match(/^\s*/)?.[0] ?? "";
  const trail = sql.match(/\s*$/)?.[0] ?? "";
  const body = sql.trim();
  if (!body) return sql;
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
  if (indent) out = out.split("\n").map((l, i) => (i === 0 || !l ? l : indent + l)).join("\n");
  return lead + out + trail;
}
