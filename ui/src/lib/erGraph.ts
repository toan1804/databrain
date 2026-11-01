// ER diagram of a schema as a lineage graph, so the lineage layout, SVG and
// exports draw it: tables are nodes, each foreign key column pair is an edge
// from the referenced (parent) column to the referencing (child) column.
import type { LineageEdge, LineageGraph, LineageNode, TableColumns } from "./types";

export interface ErGraph {
  graph: LineageGraph;
  /** Foreign keys counted once per constraint (not per column pair). */
  foreignKeys: number;
  /** Tables of other schemas that a foreign key points to (drawn dashed, no columns). */
  external: string[];
}

const nodeId = (schema: string, table: string) => `${schema}.${table}`;

export function erGraph(schema: string, tables: TableColumns[]): ErGraph {
  const nodes = new Map<string, LineageNode>();
  for (const t of tables) {
    // Self-references don't draw as edges, so the column says so.
    const selfRef = new Map<string, string>();
    for (const fk of t.foreign_keys ?? []) {
      if (fk.ref_schema !== schema || fk.ref_table !== t.table) continue;
      fk.columns.forEach((c, i) => selfRef.set(c, fk.ref_columns[i] ?? ""));
    }
    nodes.set(nodeId(schema, t.table), {
      id: nodeId(schema, t.table),
      kind: "table",
      name: t.table,
      columns: t.columns.map((c) => {
        const self = selfRef.get(c.name);
        const type = `${c.is_primary_key ? "PK " : ""}${c.data_type}`;
        return { name: c.name, expr: self !== undefined ? `${type} ↺ ${self}` : type };
      }),
      columns_known: true,
      written: false,
    });
  }
  const external: string[] = [];
  const edges: LineageEdge[] = [];
  let foreignKeys = 0;
  for (const t of tables) {
    const child = nodeId(schema, t.table);
    for (const fk of t.foreign_keys ?? []) {
      foreignKeys++;
      const parent = nodeId(fk.ref_schema, fk.ref_table);
      if (!nodes.has(parent)) {
        // Another schema's table: a placeholder box listing the referenced columns.
        nodes.set(parent, { id: parent, kind: "table", name: parent, columns: [], columns_known: false, written: false });
        external.push(parent);
      }
      const p = nodes.get(parent)!;
      if (!p.columns_known) for (const c of fk.ref_columns) if (!p.columns.some((x) => x.name === c)) p.columns.push({ name: c });
      const detail = `${t.table}(${fk.columns.join(", ")}) → ${fk.ref_schema === schema ? "" : `${fk.ref_schema}.`}${fk.ref_table}(${fk.ref_columns.join(", ")})`;
      fk.columns.forEach((c, i) => {
        edges.push({ from: parent, from_column: fk.ref_columns[i], to: child, to_column: c, kind: "direct", detail });
      });
    }
  }
  return {
    graph: { nodes: [...nodes.values()], edges, statements: [], warnings: [], partial: false, unknown_tables: [] },
    foreignKeys,
    external,
  };
}
