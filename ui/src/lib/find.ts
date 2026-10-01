// Find in a result grid: values, optionally in one column, plus column names.

export interface FindPlan {
  /** Text searched in cell values ("" = none). */
  term: string;
  /** Column indexes to search (null = all). */
  columns: number[] | null;
  /** Columns whose *name* matches (for "jump to column"). */
  nameMatches: number[];
}

/**
 * `scope` is the column picked in the find bar (null = all). A query of the
 * form `column: value` (or `column=value`) searches that column only when
 * `column` is a column name; otherwise the whole query is the value.
 */
export function planFind(query: string, columnNames: string[], scope: number | null): FindPlan {
  const q = query.trim();
  let term = q;
  let columns: number[] | null = scope === null ? null : [scope];
  const m = /^([^:=]+?)\s*[:=]\s*(.*)$/.exec(q);
  if (m && scope === null) {
    const idx = columnNames.findIndex((c) => c.toLowerCase() === m[1].trim().toLowerCase());
    if (idx >= 0) {
      columns = [idx];
      term = m[2].trim();
    }
  }
  const needle = (columns && term !== q ? "" : q).toLowerCase();
  const nameMatches = needle && scope === null ? columnNames.flatMap((c, i) => (c.toLowerCase().includes(needle) ? [i] : [])) : [];
  return { term, columns, nameMatches };
}
