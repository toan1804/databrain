// Layout of a lineage graph (left to right) and the column paths a click
// highlights. Pure functions: the diagram, SVG/PNG, draw.io and DOT exports
// all use the same positions.
import type { LineageEdge, LineageGraph, LineageNode } from "./types";

export const NODE_W = 230;
export const HEADER_H = 26;
export const ROW_H = 20;
const GAP_X = 130;
const GAP_Y = 28;
/** Top-to-bottom: space between ranks (rows) and between nodes of a rank. */
const GAP_RANK_TB = 80;
const GAP_CROSS_TB = 36;
const MARGIN = 24;
/** Database tables with more columns than this only list the columns an edge uses (+N more). */
const MAX_TABLE_COLUMNS = 12;

export interface LaidColumn {
  name: string;
  /** Index into the node's columns (−1 = the "+N more" row). */
  index: number;
  y: number;
}

export interface LaidNode {
  node: LineageNode;
  x: number;
  y: number;
  w: number;
  h: number;
  rank: number;
  columns: LaidColumn[];
  hidden: number;
  /** Table-level box: its columns are not listed (except traced ones). */
  collapsed: boolean;
}

/**
 * What the diagram lists. `expanded`: nodes whose columns are shown ("all"
 * = every node). `show`: column keys always listed, also in collapsed nodes
 * (the traced column's path). Without it every node is expanded.
 */
export interface Display {
  expanded: Set<string> | "all";
  show?: Set<string> | null;
}

/** An edge as drawn: ends at a shown column or the node; parallel edges merged. */
export interface ShownEdge {
  from: string;
  from_column?: string;
  to: string;
  to_column?: string;
  kind: LineageEdge["kind"];
  /** Indexes of the graph edges it stands for. */
  indexes: number[];
}

/** `lr` = sources on the left; `tb` = sources on top. */
export type Direction = "lr" | "tb";

export interface Layout {
  nodes: LaidNode[];
  byId: Map<string, LaidNode>;
  width: number;
  height: number;
  direction: Direction;
}

/** Top-left corners of nodes the user moved (diagram coordinates). */
export type Positions = Record<string, { x: number; y: number }>;

/**
 * Anchor of an edge end. Left to right: the right (out) / left (in) side, at
 * the column's row or the header. Top to bottom: the bottom (out) / top (in)
 * edge, spread across the width by column order (centre for the whole node),
 * so lines from different columns stay apart.
 */
export function anchor(l: Layout, node: string, column: string | undefined, side: "in" | "out"): { x: number; y: number } | null {
  const n = l.byId.get(node);
  if (!n) return null;
  const i = column !== undefined ? n.columns.findIndex((c) => c.name === column) : -1;
  if (l.direction === "tb") {
    const x = i < 0 ? n.x + n.w / 2 : n.x + 14 + ((n.w - 28) * (i + 0.5)) / n.columns.length;
    return { x, y: side === "out" ? n.y + n.h : n.y };
  }
  return { x: side === "out" ? n.x + n.w : n.x, y: i < 0 ? n.y + HEADER_H / 2 : n.columns[i].y };
}

export function layout(g: LineageGraph, direction: Direction = "lr", display: Display = { expanded: "all" }): Layout {
  const lr = direction === "lr";
  const nodes = g.nodes.filter((n) => n.name !== "" || n.columns.length > 0);
  const ids = new Set(nodes.map((n) => n.id));
  const edges = g.edges.filter((e) => ids.has(e.from) && ids.has(e.to) && e.from !== e.to);
  // Rank = longest path from a source (cycles are cut by the iteration cap).
  const rank = new Map(nodes.map((n) => [n.id, 0]));
  for (let pass = 0; pass < nodes.length; pass++) {
    let changed = false;
    for (const e of edges) {
      const r = rank.get(e.from)! + 1;
      if (r > rank.get(e.to)!) {
        rank.set(e.to, r);
        changed = true;
      }
    }
    if (!changed) break;
  }
  // Columns used by an edge (database tables only show those).
  const used = new Map<string, Set<string>>();
  const mark = (n: string, c?: string) => {
    if (c === undefined) return;
    if (!used.has(n)) used.set(n, new Set());
    used.get(n)!.add(c);
  };
  for (const e of edges) {
    mark(e.from, e.from_column);
    mark(e.to, e.to_column);
  }

  const laid: LaidNode[] = nodes.map((node) => {
    const open = display.expanded === "all" || display.expanded.has(node.id);
    const shown = (c: string) => !!display.show?.has(colKey(node.id, c));
    const fold = node.kind === "table" && node.columns.length > MAX_TABLE_COLUMNS;
    const u = used.get(node.id);
    const all = node.columns.map((c, index) => ({ name: c.name, index }));
    const cols = open ? all.filter((c) => !fold || u?.has(c.name) || shown(c.name)) : all.filter((c) => shown(c.name));
    const hidden = node.columns.length - cols.length;
    const rows = cols.length + (hidden > 0 ? 1 : 0);
    return {
      node,
      x: 0,
      y: 0,
      w: NODE_W,
      h: HEADER_H + Math.max(rows, 1) * ROW_H + 6,
      rank: rank.get(node.id)!,
      columns: cols.map((c) => ({ ...c, y: 0 })),
      hidden,
      collapsed: !open,
    };
  });
  const byRank = new Map<number, LaidNode[]>();
  for (const n of laid) {
    if (!byRank.has(n.rank)) byRank.set(n.rank, []);
    byRank.get(n.rank)!.push(n);
  }
  const ranks = [...byRank.keys()].sort((a, b) => a - b);
  const byId = new Map(laid.map((n) => [n.node.id, n]));
  // Nodes of a rank go down a column (lr) or along a row (tb).
  const place = (list: LaidNode[]) => {
    let c = MARGIN;
    for (const n of list) {
      if (lr) n.y = c;
      else n.x = c;
      c += (lr ? n.h + GAP_Y : n.w + GAP_CROSS_TB);
    }
  };
  const mid = (n: LaidNode) => (lr ? n.y + n.h / 2 : n.x + n.w / 2);
  for (const r of ranks) place(byRank.get(r)!);
  // Order each rank by where its inputs are (two sweeps cut most crossings).
  const preds = new Map<string, string[]>();
  for (const e of edges) {
    if (!preds.has(e.to)) preds.set(e.to, []);
    preds.get(e.to)!.push(e.from);
  }
  for (let sweep = 0; sweep < 2; sweep++) {
    for (const r of ranks.slice(1)) {
      const list = byRank.get(r)!;
      const center = (n: LaidNode) => {
        const p = (preds.get(n.node.id) ?? []).map((id) => byId.get(id)!).filter(Boolean);
        return p.length ? p.reduce((s, x) => s + mid(x), 0) / p.length : mid(n);
      };
      const c = new Map(list.map((n) => [n, center(n)]));
      list.sort((a, b) => c.get(a)! - c.get(b)!);
      place(list);
    }
  }
  // Main axis: ranks left to right (fixed width), or top to bottom (tallest node of the rank).
  const rankTop = new Map<number, number>();
  let top = MARGIN;
  for (const r of ranks) {
    rankTop.set(r, top);
    top += Math.max(...byRank.get(r)!.map((n) => n.h)) + GAP_RANK_TB;
  }
  for (const n of laid) {
    if (lr) n.x = MARGIN + n.rank * (NODE_W + GAP_X);
    else n.y = rankTop.get(n.rank)!;
  }
  return finish(laid, direction);
}

function finish(laid: LaidNode[], direction: Direction): Layout {
  let width = 0;
  let height = 0;
  for (const n of laid) {
    n.columns.forEach((c, i) => (c.y = n.y + HEADER_H + i * ROW_H + ROW_H / 2 + 3));
    width = Math.max(width, n.x + n.w + MARGIN);
    height = Math.max(height, n.y + n.h + MARGIN);
  }
  return { nodes: laid, byId: new Map(laid.map((n) => [n.node.id, n])), width, height, direction };
}

/** The layout with moved nodes at their new places (a copy; columns follow their node). */
export function withPositions(l: Layout, pos: Positions): Layout {
  if (!Object.keys(pos).length) return l;
  const nodes = l.nodes.map((n) => {
    const p = pos[n.node.id];
    return { ...n, x: p ? p.x : n.x, y: p ? p.y : n.y, columns: n.columns.map((c) => ({ ...c })) };
  });
  return finish(nodes, l.direction);
}

/** Shift so the diagram starts at the margin (nodes dragged up/left), for exports. */
export function normalize(l: Layout): Layout {
  const minX = Math.min(...l.nodes.map((n) => n.x), MARGIN);
  const minY = Math.min(...l.nodes.map((n) => n.y), MARGIN);
  if (minX >= MARGIN && minY >= MARGIN) return l;
  const nodes = l.nodes.map((n) => ({ ...n, x: n.x - minX + MARGIN, y: n.y - minY + MARGIN, columns: n.columns.map((c) => ({ ...c })) }));
  return finish(nodes, l.direction);
}

const DATA_KINDS: LineageEdge["kind"][] = ["direct", "transform", "aggregate"];

/**
 * Edges to draw: an end whose column is not listed attaches to the node, and
 * edges that then coincide are merged (data edges keep the strongest kind:
 * aggregated > computed > copied; filters and joins stay separate).
 */
export function shownEdges(g: LineageGraph, l: Layout): ShownEdge[] {
  const out = new Map<string, ShownEdge>();
  g.edges.forEach((e, i) => {
    const a = l.byId.get(e.from);
    const b = l.byId.get(e.to);
    if (!a || !b || e.from === e.to) return;
    const fc = e.from_column !== undefined && a.columns.some((c) => c.name === e.from_column) ? e.from_column : undefined;
    const tc = e.to_column !== undefined && b.columns.some((c) => c.name === e.to_column) ? e.to_column : undefined;
    const data = DATA_KINDS.includes(e.kind);
    const key = `${colKey(e.from, fc)}>${colKey(e.to, tc)}>${data ? "data" : e.kind}`;
    const have = out.get(key);
    if (!have) out.set(key, { from: e.from, from_column: fc, to: e.to, to_column: tc, kind: e.kind, indexes: [i] });
    else {
      have.indexes.push(i);
      if (data && DATA_KINDS.indexOf(e.kind) > DATA_KINDS.indexOf(have.kind)) have.kind = e.kind;
    }
  });
  return [...out.values()];
}

/** Key of a column end: `node|column` (or `node|` for the whole node). */
export const colKey = (node: string, column?: string) => `${node}|${column ?? ""}`;
const splitKey = (k: string): [string, string] => {
  const i = k.indexOf("|");
  return [k.slice(0, i), k.slice(i + 1)];
};

/**
 * Everything a column comes from and everything it feeds (and the filters /
 * joins on the way). Returns column keys and edge indexes.
 */
export function trace(g: LineageGraph, node: string, column?: string): { cols: Set<string>; edges: Set<number> } {
  const cols = new Set<string>([colKey(node, column)]);
  const edges = new Set<number>();
  const up = [colKey(node, column)];
  while (up.length) {
    const [n, c] = splitKey(up.pop()!);
    g.edges.forEach((e, i) => {
      // Into this column; or rows of a node it belongs to (filters/joins), one level.
      const into = e.to === n && ((e.to_column ?? "") === c || (e.to_column === undefined && c !== ""));
      if (!into || edges.has(i)) return;
      edges.add(i);
      const f = colKey(e.from, e.from_column);
      if (e.to_column !== undefined && !cols.has(f)) up.push(f);
      cols.add(f);
    });
  }
  const down = [colKey(node, column)];
  while (down.length) {
    const [n, c] = splitKey(down.pop()!);
    g.edges.forEach((e, i) => {
      const from = e.from === n && ((e.from_column ?? "") === c || e.from_column === undefined);
      if (!from || edges.has(i) || e.to_column === undefined) return;
      edges.add(i);
      const t = colKey(e.to, e.to_column);
      if (!cols.has(t)) down.push(t);
      cols.add(t);
    });
  }
  return { cols, edges };
}

export const EDGE_LABEL: Record<LineageEdge["kind"], string> = {
  direct: "copied",
  transform: "computed",
  aggregate: "aggregated",
  filter: "filters rows",
  join: "joins on",
};
