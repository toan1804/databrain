/**
 * The UI's one reader of SQL text: where strings, comments and quoted
 * names are, and the `results.<name>` references outside them. Completion,
 * query tips, inline suggestions, notebooks, jobs and output columns all
 * read SQL through this, so they agree on where a string ends.
 *
 * It follows the backend's reading (`connector-core` `skip_token` and
 * `referenced_results`) for what the UI needs: `--` and `/* … *\/`
 * comments, `'…'` strings with `''` (and `\'` where backslashes escape),
 * `$tag$…$tag$` dollar quotes, `"…"` and `` `…` `` names with a doubled
 * quote inside.
 */

/** A stretch of text that is not plain SQL: a comment, a string, or a quoted name. */
export interface Span {
  kind: "comment" | "string" | "name";
  start: number;
  /** Past its last character (its closing quote, or the end of the text). */
  end: number;
  /** Reached its closing mark (`false`: runs to the end of the text, or a `--` comment). */
  closed: boolean;
}

const DOLLAR = /^\$([A-Za-z_][A-Za-z0-9_]*)?\$/;

/**
 * The comments, strings and quoted names of `sql`, in order. `backslash`:
 * `\'` doesn't end a string (MySQL, BigQuery, ClickHouse…).
 */
export function sqlSpans(sql: string, backslash = false): Span[] {
  const out: Span[] = [];
  const n = sql.length;
  let i = 0;
  while (i < n) {
    const c = sql[i];
    const next = sql[i + 1];
    if (c === "-" && next === "-") {
      const j = sql.indexOf("\n", i);
      const end = j < 0 ? n : j;
      out.push({ kind: "comment", start: i, end, closed: false });
      i = end;
    } else if (c === "/" && next === "*") {
      const j = sql.indexOf("*/", i + 2);
      const end = j < 0 ? n : j + 2;
      out.push({ kind: "comment", start: i, end, closed: j >= 0 });
      i = end;
    } else if (c === "'") {
      let j = i + 1;
      while (j < n && !(sql[j] === "'" && sql[j + 1] !== "'")) j += sql[j] === "'" || (backslash && sql[j] === "\\") ? 2 : 1;
      const closed = j < n;
      const end = Math.min(n, j + 1);
      out.push({ kind: "string", start: i, end, closed });
      i = end;
    } else if (c === '"' || c === "`") {
      let j = i + 1;
      while (j < n && !(sql[j] === c && sql[j + 1] !== c)) j += sql[j] === c ? 2 : 1;
      const closed = j < n;
      const end = Math.min(n, j + 1);
      out.push({ kind: "name", start: i, end, closed });
      i = end;
    } else if (c === "$" && !/[\w$]/.test(sql[i - 1] ?? "")) {
      const m = DOLLAR.exec(sql.slice(i, i + 66));
      if (!m) {
        i++;
        continue;
      }
      const tag = m[0];
      const j = sql.indexOf(tag, i + tag.length);
      const end = j < 0 ? n : j + tag.length;
      out.push({ kind: "string", start: i, end, closed: j >= 0 });
      i = end;
    } else {
      i++;
    }
  }
  return out;
}

/**
 * `sql` with strings and comments replaced by spaces (line breaks kept, so
 * offsets and lines stay). Quoted names are kept as they are, so an
 * apostrophe in one doesn't open a string.
 */
export function blankLiterals(sql: string, backslash = false): string {
  let out = "";
  let at = 0;
  for (const s of sqlSpans(sql, backslash)) {
    if (s.kind === "name") continue;
    out += sql.slice(at, s.start) + sql.slice(s.start, s.end).replace(/[^\n]/g, " ");
    at = s.end;
  }
  return out + sql.slice(at);
}

/** Whether `pos` (a cursor) is inside a string or comment of `sql`. */
export function inStringOrComment(sql: string, pos: number, backslash = false): boolean {
  for (const s of sqlSpans(sql, backslash)) {
    if (s.start >= pos) return false;
    if (s.kind === "name") continue;
    // After a closing quote is outside; an open string, or a `--` comment, runs on to the cursor.
    if (pos < s.end || (!s.closed && pos <= s.end)) return true;
  }
  return false;
}

/** A `results.<name>` reference: the name, and where it is (its quotes included). */
export interface OutputRef {
  name: string;
  from: number;
  to: number;
  quoted: boolean;
}

/** Name characters: ASCII words, `$` after the first, and any letter outside ASCII. */
const NAME = String.raw`[A-Za-z_À-￿][\w$À-￿]*`;
const REF = new RegExp(String.raw`(?<![\w.$"])(?:results|"results")\s*\.\s*(?:"((?:[^"]|"")*)"|(${NAME}))`, "gi");

/**
 * Every `results.<name>` outside strings and comments, as DuckDB reads it:
 * `results` and the name may be quoted, and spaces or comments may sit
 * between the parts (`results /* x *\/ . sales`).
 */
export function outputRefs(sql: string): OutputRef[] {
  const clean = blankLiterals(sql);
  const out: OutputRef[] = [];
  for (const m of clean.matchAll(REF)) {
    const quoted = m[1] !== undefined;
    const name = quoted ? m[1].replace(/""/g, '"') : m[2];
    if (!name) continue;
    const raw = quoted ? `"${m[1]}"` : m[2];
    const to = m.index + m[0].length;
    out.push({ name, from: to - raw.length, to, quoted });
  }
  return out;
}

/** The output names `sql` reads as `results.<name>`, each once (first spelling), in order. */
export function outputNames(sql: string): string[] {
  const out: string[] = [];
  for (const r of outputRefs(sql)) if (!out.some((x) => x.toLowerCase() === r.name.toLowerCase())) out.push(r.name);
  return out;
}

/**
 * `sql` with `results.<from>` (any case) read as `results.<to>`: only real
 * references, not a `results.from` inside a string or comment. The new
 * name is written plain when it is a plain identifier (DuckDB reads both
 * the same), quoted otherwise.
 */
export function renameOutputRefs(sql: string, from: string, to: string): string {
  const want = from.toLowerCase();
  const written = /^[A-Za-z_][A-Za-z0-9_]*$/.test(to) ? to : `"${to.replace(/"/g, '""')}"`;
  let out = sql;
  for (const r of outputRefs(sql).reverse()) {
    if (r.name.toLowerCase() === want) out = out.slice(0, r.from) + written + out.slice(r.to);
  }
  return out;
}
