// AI table access rules (connection → AI): LIKE patterns of tables the AI
// may read and write. Mirrors crates/ai/src/access.rs for the dialog's
// pattern parsing and "try a table" check; the backend enforces them.

/** A pattern without surrounding quotes and spaces, lowercased. */
function clean(p: string): string {
  return p.trim().replace(/^['"`]+|['"`]+$/g, "").trim().toLowerCase();
}

/** `'catalog_read%', '%schema_can_read%'` (or one per line) → patterns. */
export function parsePatterns(text: string): string[] {
  return text.split(/[,\n;]/).map(clean).filter(Boolean);
}

/** Patterns as the user types them back. */
export function formatPatterns(list: string[] | undefined): string {
  return (list ?? []).map((p) => `'${p}'`).join(", ");
}

/** SQL LIKE (`%`, `_`, also `*`, `?`), case-insensitive. */
export function like(pattern: string, text: string): boolean {
  const p = [...pattern.toLowerCase()];
  const t = [...text.toLowerCase()];
  let pi = 0;
  let ti = 0;
  let star = -1;
  let mark = 0;
  while (ti < t.length) {
    if (pi < p.length && (p[pi] === "_" || p[pi] === "?" || p[pi] === t[ti])) {
      pi++;
      ti++;
    } else if (pi < p.length && (p[pi] === "%" || p[pi] === "*")) {
      star = pi++;
      mark = ti;
    } else if (star >= 0) {
      pi = star + 1;
      ti = ++mark;
    } else return false;
  }
  while (pi < p.length && (p[pi] === "%" || p[pi] === "*")) pi++;
  return pi === p.length;
}

/** Does a pattern match `a.b.c` or a shorter form (`b.c`, `c`)? */
export function matchesTable(patterns: string[], table: string): string | null {
  const parts = table.split(".").map((x) => x.trim().replace(/^["`[]|["`\]]$/g, "")).filter(Boolean);
  const forms = parts.map((_, i) => parts.slice(i).join("."));
  return patterns.map(clean).find((p) => p && forms.some((f) => like(p, f))) ?? null;
}

/** Whether the AI may read or write `table` (a full name), and why not. */
export function tableAccess(allow: string[], deny: string[], table: string): { ok: boolean; why: string } {
  const d = matchesTable(deny, table);
  if (d) return { ok: false, why: `matches '${d}' (deny)` };
  if (allow.some((a) => clean(a))) {
    const a = matchesTable(allow, table);
    return a ? { ok: true, why: `matches '${a}'` } : { ok: false, why: "matches no allow pattern" };
  }
  return { ok: true, why: "no allow list: every table not denied" };
}
