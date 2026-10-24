// Pending edits of a result grid: changed cells (by original row id), new
// rows and deleted rows, turned into the changes the backend writes as SQL.

/** A cell's value; null = SQL NULL. */
export type CellText = string | null;

export interface PendingEdits {
  /** Changed cells per original row id, with the row's key values as fetched. */
  updates: Map<number, { key: CellText[]; values: Map<number, CellText> }>;
  /** Rows to delete (original row id → key values). */
  deletes: Map<number, CellText[]>;
  /** New rows: their typed values by column. */
  inserts: Map<number, CellText>[];
}

export const emptyEdits = (): PendingEdits => ({ updates: new Map(), deletes: new Map(), inserts: [] });

/** How many changes there are (cells, new rows, deleted rows). */
export function editCount(e: PendingEdits): number {
  let n = e.deletes.size + e.inserts.length;
  for (const u of e.updates.values()) n += u.values.size;
  return n;
}

/** Change a fetched cell; typing the original value back removes the change. */
export function setCell(e: PendingEdits, rowId: number, key: CellText[], col: number, original: CellText, value: CellText): PendingEdits {
  const updates = new Map(e.updates);
  const cur = updates.get(rowId);
  const values = new Map(cur?.values ?? []);
  if (value === original) values.delete(col);
  else values.set(col, value);
  if (values.size) updates.set(rowId, { key: cur?.key ?? key, values });
  else updates.delete(rowId);
  return { ...e, updates };
}

/** Change a cell of a new row. */
export function setNewCell(e: PendingEdits, index: number, col: number, value: CellText): PendingEdits {
  const inserts = e.inserts.slice();
  const row = new Map(inserts[index] ?? []);
  row.set(col, value);
  inserts[index] = row;
  return { ...e, inserts };
}

export const addRow = (e: PendingEdits): PendingEdits => ({ ...e, inserts: [...e.inserts, new Map()] });

export const removeNewRow = (e: PendingEdits, index: number): PendingEdits => ({ ...e, inserts: e.inserts.filter((_, i) => i !== index) });

/** Mark fetched rows for deletion (their cell changes are dropped), or unmark them. */
export function toggleDelete(e: PendingEdits, rows: { id: number; key: CellText[] }[]): PendingEdits {
  const deletes = new Map(e.deletes);
  const updates = new Map(e.updates);
  const all = rows.every((r) => deletes.has(r.id));
  for (const r of rows) {
    if (all) deletes.delete(r.id);
    else {
      deletes.set(r.id, r.key);
      updates.delete(r.id);
    }
  }
  return { ...e, deletes, updates };
}

/** Drop the changes of these fetched rows. */
export function revertRows(e: PendingEdits, ids: number[]): PendingEdits {
  const updates = new Map(e.updates);
  const deletes = new Map(e.deletes);
  for (const id of ids) {
    updates.delete(id);
    deletes.delete(id);
  }
  return { ...e, updates, deletes };
}

export type RowChange =
  | { op: "update"; key: CellText[]; values: { column: number; value: CellText }[] }
  | { op: "insert"; values: { column: number; value: CellText }[] }
  | { op: "delete"; key: CellText[] };

/** The changes in apply order: updates, new rows, deletions. */
export function toChanges(e: PendingEdits): RowChange[] {
  const out: RowChange[] = [];
  for (const u of e.updates.values()) out.push({ op: "update", key: u.key, values: [...u.values].map(([column, value]) => ({ column, value })) });
  for (const r of e.inserts) out.push({ op: "insert", values: [...r].map(([column, value]) => ({ column, value })) });
  for (const key of e.deletes.values()) out.push({ op: "delete", key });
  return out;
}
