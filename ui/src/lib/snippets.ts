// SQL snippets: a prefix typed in the editor expands to a template whose
// `${name}` fields are filled in turn (Tab / Shift-Tab), CodeMirror's
// snippet syntax. Built-in ones plus the user's (setting `sql_snippets`).

export interface Snippet {
  /** What is typed to offer it (`sel`). */
  prefix: string;
  /** The template: `${field}` placeholders, `${}` for the final cursor. */
  body: string;
  description?: string;
}

export const BUILTIN_SNIPPETS: Snippet[] = [
  { prefix: "sel", body: "SELECT ${columns}\nFROM ${table}\nWHERE ${condition}", description: "SELECT … FROM … WHERE" },
  { prefix: "selc", body: "SELECT count(*)\nFROM ${table}\nWHERE ${condition}", description: "count rows" },
  { prefix: "seld", body: "SELECT ${column}, count(*) AS n\nFROM ${table}\nGROUP BY ${column}\nORDER BY n DESC", description: "values and their counts" },
  { prefix: "ins", body: "INSERT INTO ${table} (${columns})\nVALUES (${values})", description: "INSERT … VALUES" },
  { prefix: "upd", body: "UPDATE ${table}\nSET ${column} = ${value}\nWHERE ${condition}", description: "UPDATE … SET … WHERE" },
  { prefix: "del", body: "DELETE FROM ${table}\nWHERE ${condition}", description: "DELETE … WHERE" },
  { prefix: "with", body: "WITH ${name} AS (\n  ${query}\n)\nSELECT *\nFROM ${name}", description: "common table expression" },
  { prefix: "join", body: "JOIN ${table} ${alias} ON ${alias}.${column} = ${other}", description: "JOIN … ON" },
  { prefix: "lj", body: "LEFT JOIN ${table} ${alias} ON ${alias}.${column} = ${other}", description: "LEFT JOIN … ON" },
  { prefix: "case", body: "CASE\n  WHEN ${condition} THEN ${value}\n  ELSE ${other}\nEND", description: "CASE WHEN … END" },
  { prefix: "over", body: "${function}() OVER (PARTITION BY ${columns} ORDER BY ${order})", description: "window function" },
  { prefix: "dup", body: "SELECT ${columns}, count(*) AS n\nFROM ${table}\nGROUP BY ${columns}\nHAVING count(*) > 1", description: "find duplicate rows" },
];

/** User snippets from a setting value (bad entries are dropped). */
export function parseSnippets(v: unknown): Snippet[] {
  if (!Array.isArray(v)) return [];
  return v
    .filter((s): s is Snippet => !!s && typeof s === "object" && typeof (s as Snippet).prefix === "string" && typeof (s as Snippet).body === "string")
    .map((s) => ({ prefix: s.prefix.trim(), body: s.body, description: typeof s.description === "string" ? s.description : undefined }))
    .filter((s) => /^[\w$#@.-]+$/.test(s.prefix) && s.body.trim() !== "");
}

/** The user's snippets first; a user snippet replaces a built-in one with the same prefix. */
export function allSnippets(user: Snippet[]): Snippet[] {
  const mine = new Set(user.map((s) => s.prefix.toLowerCase()));
  return [...user, ...BUILTIN_SNIPPETS.filter((s) => !mine.has(s.prefix.toLowerCase()))];
}

/** Snippets offered for the word being typed (prefix match, exact ones first). */
export function matchSnippets(word: string, snippets: Snippet[]): Snippet[] {
  const w = word.toLowerCase();
  if (!w) return [];
  const hits = snippets.filter((s) => s.prefix.toLowerCase().startsWith(w));
  return hits.sort((a, b) => Number(b.prefix.toLowerCase() === w) - Number(a.prefix.toLowerCase() === w));
}

/** The template as text with the field names (preview and the completion's info). */
export function snippetPreview(body: string): string {
  return body.replace(/[$#]\{(?:\d+:)?([^}]*)\}/g, (_m, name: string) => name);
}
