// "How is this column computed?" for the lineage side panel: one formula in
// terms of database columns, then the steps from the source tables to the
// column, in data-flow order. Pure: tested without a UI.
import { colKey } from "./lineageLayout";
import type { LineageEdge, LineageGraph, LineageNode, LineageRange } from "./types";

const DATA = new Set<LineageEdge["kind"]>(["direct", "transform", "aggregate"]);
const MAX_FORMULA = 240;

export interface StepColumn {
  name: string;
  /** Local expression (`sum(p.amount)`); absent = copied / read as is. */
  expr?: string;
  span?: LineageRange;
}

export interface StepCondition {
  kind: "filter" | "join";
  detail: string;
  span?: LineageRange;
}

export interface Step {
  node: LineageNode;
  /** Columns of this node on the path: computed ones and those passed through. */
  computed: StepColumn[];
  passed: StepColumn[];
  /** Joins and filters of this node (which rows reach the column). */
  conditions: StepCondition[];
}

export interface ColumnExplanation {
  /** The column in terms of database columns: `sum(sales.orders.amount)`. */
  formula: string;
  /** Database columns the value is computed from. */
  sources: string[];
  /** Database columns that only decide which rows count (joins, filters). */
  rowColumns: string[];
  /** Source tables first, the column's own node last. */
  steps: Step[];
}

const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/** Replace `ref` as a whole reference (not inside a longer name). */
function substitute(text: string, ref: string, by: string): string {
  if (!ref) return text;
  const re = new RegExp(`(?<![\\w."\`\\]])${escape(ref)}(?![\\w"\`\\[])`, "g");
  return text.replace(re, by.replace(/\$/g, "$$$$"));
}

/**
 * `sql` is the analysed script (edge spans point into it, so the text an
 * expression uses for each input can be found and replaced).
 */
export function explainColumn(g: LineageGraph, sql: string, node: string, column?: string): ColumnExplanation | null {
  const byId = new Map(g.nodes.map((n) => [n.id, n]));
  const target = byId.get(node);
  if (!target) return null;
  const into = new Map<string, LineageEdge[]>();
  for (const e of g.edges) {
    if (!DATA.has(e.kind) || e.to_column === undefined) continue;
    const k = colKey(e.to, e.to_column);
    if (!into.has(k)) into.set(k, []);
    into.get(k)!.push(e);
  }
  const short = (n: LineageNode) => n.name;

  // Columns on the path with their distance from the target (longest), and the database sources.
  const depth = new Map<string, number>();
  const sources = new Set<string>();
  const visit = (id: string, col: string | undefined, d: number, path: Set<string>) => {
    const k = colKey(id, col);
    if (path.has(k)) return;
    if ((depth.get(k) ?? -1) >= d) return;
    depth.set(k, d);
    const ins = col !== undefined ? (into.get(k) ?? []) : [];
    const n = byId.get(id)!;
    if (!ins.length && n.kind === "table") sources.add(`${short(n)}.${col ?? "*"}`);
    const next = new Set(path).add(k);
    for (const e of ins) if (byId.has(e.from)) visit(e.from, e.from_column, d + 1, next);
  };
  visit(node, column, 0, new Set());

  // Formula: substitute each input's own formula into the expression, bottom up.
  const memo = new Map<string, string>();
  const formula = (id: string, col: string | undefined, path: Set<string>): string => {
    const k = colKey(id, col);
    const n = byId.get(id)!;
    const plain = `${short(n)}.${col ?? "*"}`;
    if (memo.has(k)) return memo.get(k)!;
    if (path.has(k) || col === undefined) return plain;
    const ins = (into.get(k) ?? []).filter((e) => byId.has(e.from));
    const next = new Set(path).add(k);
    const c = n.columns.find((x) => x.name === col);
    let out: string;
    if (!ins.length) out = n.kind === "table" ? plain : (c?.expr ?? plain);
    else if (!c?.expr) out = ins.length === 1 ? formula(ins[0].from, ins[0].from_column, next) : plain;
    else {
      out = c.expr;
      for (const e of ins) {
        const ref = e.span ? sql.slice(e.span.start, e.span.end) : "";
        const sub = formula(e.from, e.from_column, next);
        // Brackets only where needed: not for a plain column, not as a lone argument `f(x)`.
        const lone = ref && new RegExp(`\\(\\s*${escape(ref)}\\s*\\)`).test(out);
        const wrapped = /^[\w."`[\]]+$/.test(sub) || lone ? sub : `(${sub})`;
        const replaced = substitute(out, ref, wrapped);
        if (replaced.length > MAX_FORMULA) continue;
        out = replaced;
      }
    }
    memo.set(k, out);
    return out;
  };
  const f = formula(node, column, new Set());

  // Steps: nodes on the path, farthest (sources) first.
  const perNode = new Map<string, { d: number; cols: string[] }>();
  for (const [k, d] of depth) {
    const i = k.indexOf("|");
    const id = k.slice(0, i);
    const col = k.slice(i + 1);
    const s = perNode.get(id) ?? { d: 0, cols: [] };
    s.d = Math.max(s.d, d);
    if (col && !s.cols.includes(col)) s.cols.push(col);
    perNode.set(id, s);
  }
  const rowColumns = new Set<string>();
  const steps: Step[] = [...perNode.entries()]
    .sort((a, b) => b[1].d - a[1].d)
    .map(([id, { cols }]) => {
      const n = byId.get(id)!;
      const computed: StepColumn[] = [];
      const passed: StepColumn[] = [];
      for (const name of cols) {
        const c = n.columns.find((x) => x.name === name);
        (c?.expr ? computed : passed).push({ name, expr: c?.expr, span: c?.span });
      }
      const conds = new Map<string, StepCondition>();
      for (const e of g.edges) {
        if (e.to !== id || e.to_column !== undefined || DATA.has(e.kind)) continue;
        const from = byId.get(e.from);
        if (from) rowColumns.add(`${short(from)}.${e.from_column ?? "*"}`);
        const detail = e.detail ?? (e.kind === "join" ? "JOIN" : "Filter");
        if (!conds.has(detail)) conds.set(detail, { kind: e.kind as "filter" | "join", detail, span: e.span });
      }
      const conditions = [...conds.values()].sort((a, b) => Number(a.kind === "filter") - Number(b.kind === "filter"));
      return { node: n, computed, passed, conditions };
    })
    // A step that only passes columns through, with no joins/filters, says nothing: fold it.
    .filter((s, i, all) => s.node.kind === "table" || s.computed.length > 0 || s.conditions.length > 0 || i === all.length - 1);
  return { formula: f, sources: [...sources], rowColumns: [...rowColumns].filter((c) => !sources.has(c)), steps };
}

// ------------------------------------------------------------------ flow (side panel)

/** One box of the column flow: a column, how it's computed from the boxes below it. */
export interface FlowNode {
  node: LineageNode;
  column?: string;
  span?: LineageRange;
  /** Expression on the arrow to the inputs (`sum(n.net_amount)`); absent = copied. */
  expr?: string;
  /** How the inputs are used: strongest kind among them. */
  via?: "direct" | "transform" | "aggregate";
  /** Joins / filters of this node, shown where the arrows to the inputs meet. */
  conditions: StepCondition[];
  /** Nodes that only passed the value through (folded into the arrow). */
  through: string[];
  children: FlowNode[];
  /** Reached again: not expanded a second time. */
  repeat?: boolean;
}

/** `LEFT JOIN customers c ON c.id = n.id` → `left join customers c on c.id = n.id`. */
export function conditionText(c: StepCondition): string {
  return c.detail
    .replace(/^((?:LEFT|RIGHT|FULL|CROSS|SEMI|ANTI|ASOF|INNER|OUTER)\s+)*(JOIN|APPLY)\b/i, (m) => m.toLowerCase())
    .replace(/\s(ON|USING)\s/, (m) => m.toLowerCase())
    .replace(/^(WHERE|HAVING|QUALIFY|PREWHERE)\b/, (m) => m.toLowerCase());
}

/**
 * The column on top, its inputs below (side by side when several), each with
 * theirs, down to the database tables. Columns that are only copied through
 * a CTE/subquery without joins or filters are folded into the arrow ("via …").
 */
export function columnFlow(g: LineageGraph, node: string, column?: string, maxDepth = 30): FlowNode | null {
  const byId = new Map(g.nodes.map((n) => [n.id, n]));
  const into = new Map<string, LineageEdge[]>();
  const conds = new Map<string, StepCondition[]>();
  for (const e of g.edges) {
    if (DATA.has(e.kind) && e.to_column !== undefined) {
      const k = colKey(e.to, e.to_column);
      if (!into.has(k)) into.set(k, []);
      into.get(k)!.push(e);
    } else if (!DATA.has(e.kind) && e.to_column === undefined) {
      const list = conds.get(e.to) ?? [];
      const detail = e.detail ?? (e.kind === "join" ? "JOIN" : "Filter");
      if (!list.some((c) => c.detail === detail)) list.push({ kind: e.kind as "filter" | "join", detail, span: e.span });
      conds.set(e.to, list);
    }
  }
  for (const l of conds.values()) l.sort((a, b) => Number(a.kind === "filter") - Number(b.kind === "filter"));
  const inputs = (id: string, col?: string) => (col === undefined ? [] : (into.get(colKey(id, col)) ?? []).filter((e) => byId.has(e.from)));
  const seen = new Set<string>();
  const condShown = new Set<string>();
  const rank = { direct: 0, transform: 1, aggregate: 2 } as const;

  const build = (id: string, col: string | undefined, depth: number): FlowNode => {
    const n = byId.get(id)!;
    const c = col !== undefined ? n.columns.find((x) => x.name === col) : undefined;
    const f: FlowNode = { node: n, column: col, span: c?.span ?? n.span, expr: c?.expr, conditions: [], through: [], children: [] };
    const key = colKey(id, col);
    if (seen.has(key) || depth > maxDepth) return { ...f, repeat: true };
    seen.add(key);
    // A node's joins/filters are shown once (where it first appears).
    if (!condShown.has(id)) {
      condShown.add(id);
      f.conditions = conds.get(id) ?? [];
    }
    const ins = inputs(id, col);
    for (const e of ins) {
      f.via = !f.via || rank[e.kind as keyof typeof rank] > rank[f.via] ? (e.kind as FlowNode["via"]) : f.via;
      // Fold plain copies through nodes that don't join or filter.
      let from = e.from;
      let fcol = e.from_column;
      const through: string[] = [];
      for (let hop = 0; hop < 50; hop++) {
        const m = byId.get(from)!;
        const mc = fcol !== undefined ? m.columns.find((x) => x.name === fcol) : undefined;
        const next = inputs(from, fcol);
        if (m.kind === "table" || mc?.expr || (conds.get(from)?.length ?? 0) > 0 || next.length !== 1 || next[0].kind !== "direct" || seen.has(colKey(from, fcol))) break;
        through.push(m.name);
        from = next[0].from;
        fcol = next[0].from_column;
      }
      const child = build(from, fcol, depth + 1);
      child.through = through;
      f.children.push(child);
    }
    return f;
  };
  return byId.has(node) ? build(node, column, 0) : null;
}
