// Lineage diagram as SVG (also what the tab shows), draw.io, Mermaid,
// Graphviz DOT and JSON. Pure string builders over the shared layout.
import { HEADER_H, ROW_H, anchor, colKey, layout, normalize, shownEdges, type LaidNode, type Layout, type ShownEdge } from "./lineageLayout";
import type { LineageEdge, LineageGraph, LineageNode } from "./types";

export interface DiagramColors {
  bg: string;
  panel: string;
  header: string;
  border: string;
  text: string;
  muted: string;
  accent: string;
  warning: string;
  font: string;
}

/** Fixed colours for exports (readable on white, like most docs). */
export const LIGHT: DiagramColors = {
  bg: "#ffffff",
  panel: "#ffffff",
  header: "#f1f5f9",
  border: "#cbd5e1",
  text: "#0f172a",
  muted: "#64748b",
  accent: "#2563eb",
  warning: "#d97706",
  font: "ui-monospace, Menlo, Consolas, monospace",
};

const xml = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&apos;");

export function kindLabel(n: LineageNode): string {
  if (n.kind === "table") return n.written ? "written" : n.columns_known ? "table" : "table · columns not cached";
  return { cte: "CTE", subquery: "subquery", set_op: "set operation", result: "result" }[n.kind];
}

/** Edge style per kind: colour, dash, width. */
function edgeStyle(e: LineageEdge | ShownEdge, c: DiagramColors): { stroke: string; dash: string; width: number } {
  switch (e.kind) {
    case "direct":
      return { stroke: c.muted, dash: "", width: 1.4 };
    case "transform":
      return { stroke: c.accent, dash: "", width: 1.4 };
    case "aggregate":
      return { stroke: c.accent, dash: "", width: 2.6 };
    case "filter":
      return { stroke: c.warning, dash: "5 4", width: 1.3 };
    case "join":
      return { stroke: c.warning, dash: "2 3", width: 1.3 };
  }
}

function path(a: { x: number; y: number }, b: { x: number; y: number }, tb = false): string {
  if (tb) {
    // Out of the bottom, into the top; a target above (dragged) gets a wider S-curve.
    const dy = b.y > a.y + 40 ? Math.max(40, (b.y - a.y) / 2) : Math.max(70, Math.abs(b.x - a.x) / 3);
    return `M${a.x},${a.y} C${a.x},${a.y + dy} ${b.x},${b.y - dy} ${b.x},${b.y}`;
  }
  // Lines leave a column on the right and enter on the left; a target to the
  // left (dragged nodes) gets a wider S-curve.
  const dx = b.x > a.x + 40 ? Math.max(40, (b.x - a.x) / 2) : Math.max(70, Math.abs(b.y - a.y) / 3);
  return `M${a.x},${a.y} C${a.x + dx},${a.y} ${b.x - dx},${b.y} ${b.x},${b.y}`;
}

const clip = (s: string, n: number) => (s.length > n ? s.slice(0, n - 1) + "…" : s);

/** Text of the row under the listed columns ("" = none). */
function moreLabel(n: LaidNode): string {
  if (n.hidden <= 0) return "";
  const s = n.hidden === 1 ? "" : "s";
  if (!n.collapsed || n.columns.length) return `+${n.hidden} more column${s}`;
  return `${n.hidden} column${s}`;
}

export interface SvgOptions {
  colors?: DiagramColors;
  /** Highlighted column keys / edge indexes (others are dimmed). */
  focus?: { cols: Set<string>; edges: Set<number> } | null;
  /** Inner markup only (the tab wraps it in its own pan/zoom group). */
  inner?: boolean;
}

/** The diagram. Columns carry `data-node` / `data-col` for clicks. */
export function toSvg(g: LineageGraph, l: Layout = layout(g), opts: SvgOptions = {}): string {
  const c = opts.colors ?? LIGHT;
  const f = opts.focus;
  const dim = (on: boolean) => (f && !on ? ' opacity="0.18"' : "");
  const out: string[] = [];
  const traced = new Set(f ? [...f.cols].map((k) => k.slice(0, k.indexOf("|"))) : []);
  for (const e of shownEdges(g, l)) {
    const a = anchor(l, e.from, e.from_column, "out");
    const b = anchor(l, e.to, e.to_column, "in");
    if (!a || !b) continue;
    const s = edgeStyle(e, c);
    const n = e.indexes.length;
    out.push(
      `<path d="${path(a, b, l.direction === "tb")}" fill="none" stroke="${s.stroke}" stroke-width="${s.width}"${s.dash ? ` stroke-dasharray="${s.dash}"` : ""}${dim(e.indexes.some((i) => f?.edges.has(i)))}><title>${xml(e.kind + (n > 1 ? ` · ${n} column links` : ""))}</title></path>`,
    );
  }
  for (const n of l.nodes) {
    const node = n.node;
    const strong = node.kind === "result" || node.written;
    const on = !f || traced.has(node.id);
    out.push(`<g data-node="${xml(node.id)}"${dim(on)}${opts.inner ? ' style="cursor:move"' : ""}>`);
    out.push(
      `<rect x="${n.x}" y="${n.y}" width="${n.w}" height="${n.h}" rx="6" fill="${c.panel}" stroke="${strong ? c.accent : c.border}" stroke-width="${strong ? 1.6 : 1}"${node.kind === "table" && !node.columns_known ? ' stroke-dasharray="4 3"' : ""}/>`,
    );
    out.push(`<path d="M${n.x},${n.y + 6} a6,6 0 0 1 6,-6 h${n.w - 12} a6,6 0 0 1 6,6 v${HEADER_H - 6} h-${n.w} z" fill="${c.header}"/>`);
    // Header: click to show/hide the columns (in the tab).
    const chevron = opts.inner ? (n.collapsed ? "▸ " : "▾ ") : "";
    out.push(
      `<g data-head="1"><rect x="${n.x}" y="${n.y}" width="${n.w}" height="${HEADER_H}" fill="transparent"/><text x="${n.x + 8}" y="${n.y + 17}" font-family="${xml(c.font)}" font-size="12" font-weight="600" fill="${c.text}">${xml(chevron + clip(node.name, 25))}<title>${xml(node.name + (opts.inner ? (n.collapsed ? " · click to show columns" : " · click to hide columns") : ""))}</title></text></g>`,
    );
    out.push(`<text x="${n.x + n.w - 8}" y="${n.y + 17}" text-anchor="end" font-family="${xml(c.font)}" font-size="9.5" fill="${c.muted}">${xml(kindLabel(node).split(" ·")[0])}</text>`);
    for (const col of n.columns) {
      const lc = node.columns[col.index];
      const hot = f?.cols.has(colKey(node.id, col.name));
      out.push(
        `<g data-node="${xml(node.id)}" data-col="${xml(col.name)}" style="cursor:pointer">` +
          `<rect x="${n.x + 1}" y="${col.y - ROW_H / 2}" width="${n.w - 2}" height="${ROW_H}" fill="${hot ? c.accent : "transparent"}" fill-opacity="${hot ? 0.14 : 0}"/>` +
          `<text x="${n.x + 10}" y="${col.y + 4}" font-family="${xml(c.font)}" font-size="11.5" fill="${c.text}">${xml(clip(col.name, lc?.expr ? 18 : 30))}</text>` +
          (lc?.expr ? `<text x="${n.x + n.w - 8}" y="${col.y + 4}" text-anchor="end" font-family="${xml(c.font)}" font-size="10" fill="${c.muted}">${xml(clip(lc.expr, 16))}</text>` : "") +
          `<title>${xml(col.name + (lc?.expr ? ` = ${lc.expr}` : ""))}</title></g>`,
      );
    }
    const more = moreLabel(n);
    if (more) {
      const y = n.y + HEADER_H + n.columns.length * ROW_H + ROW_H / 2 + 7;
      const hint = opts.inner && n.collapsed ? " · click to show" : "";
      out.push(`<text data-head="1" x="${n.x + 10}" y="${y}" font-family="${xml(c.font)}" font-size="10.5" fill="${c.muted}">${xml(more + hint)}</text>`);
    }
    out.push("</g>");
  }
  const body = out.join("\n");
  if (opts.inner) return body;
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${l.width}" height="${l.height}" viewBox="0 0 ${l.width} ${l.height}">\n<rect width="100%" height="100%" fill="${c.bg}"/>\n${body}\n</svg>\n`;
}

/** draw.io / diagrams.net file: one container per node, a row per column, edges between rows. */
export function toDrawio(g: LineageGraph, l: Layout = layout(g)): string {
  const cells: string[] = ['<mxCell id="0"/>', '<mxCell id="1" parent="0"/>'];
  const colId = new Map<string, string>();
  let k = 0;
  for (const n of l.nodes) {
    const node = n.node;
    const id = `node${k++}`;
    const stroke = node.kind === "result" || node.written ? LIGHT.accent : LIGHT.border;
    const dashed = node.kind === "table" && !node.columns_known ? "dashed=1;" : "";
    cells.push(
      `<mxCell id="${id}" value="${xml(node.name)}" style="swimlane;fontStyle=1;startSize=${HEADER_H};rounded=1;arcSize=6;html=0;fillColor=${LIGHT.header};strokeColor=${stroke};${dashed}fontFamily=Courier New;" vertex="1" parent="1"><mxGeometry x="${n.x}" y="${n.y}" width="${n.w}" height="${n.h}" as="geometry"/></mxCell>`,
    );
    colId.set(colKey(node.id), id);
    n.columns.forEach((c, i) => {
      const cid = `${id}c${i}`;
      const lc = node.columns[c.index];
      const label = lc?.expr ? `${c.name} = ${lc.expr}` : c.name;
      cells.push(
        `<mxCell id="${cid}" value="${xml(label)}" style="text;align=left;verticalAlign=middle;spacingLeft=8;overflow=hidden;fontFamily=Courier New;fontSize=11;fillColor=#ffffff;" vertex="1" parent="${id}"><mxGeometry y="${HEADER_H + i * ROW_H}" width="${n.w}" height="${ROW_H}" as="geometry"/></mxCell>`,
      );
      colId.set(colKey(node.id, c.name), cid);
    });
    if (moreLabel(n)) {
      cells.push(
        `<mxCell id="${id}more" value="${xml(moreLabel(n))}" style="text;align=left;spacingLeft=8;fontColor=${LIGHT.muted};fontSize=10;" vertex="1" parent="${id}"><mxGeometry y="${HEADER_H + n.columns.length * ROW_H}" width="${n.w}" height="${ROW_H}" as="geometry"/></mxCell>`,
      );
    }
  }
  shownEdges(g, l).forEach((e, i) => {
    const s = colId.get(colKey(e.from, e.from_column)) ?? colId.get(colKey(e.from));
    const t = colId.get(colKey(e.to, e.to_column)) ?? colId.get(colKey(e.to));
    if (!s || !t) return;
    const st = edgeStyle(e, LIGHT);
    const dash = st.dash ? "dashed=1;" : "";
    cells.push(
      `<mxCell id="e${i}" value="${e.kind === "direct" ? "" : xml(e.kind)}" style="${l.direction === "tb" ? "edgeStyle=orthogonalEdgeStyle;rounded=1;exitX=0.5;exitY=1;entryX=0.5;entryY=0;" : "edgeStyle=entityRelationEdgeStyle;curved=1;"}endArrow=block;endSize=5;strokeColor=${st.stroke};strokeWidth=${st.width};${dash}fontSize=9;fontColor=${LIGHT.muted};" edge="1" parent="1" source="${s}" target="${t}"><mxGeometry relative="1" as="geometry"/></mxCell>`,
    );
  });
  return `<mxfile host="DataBrain"><diagram name="Lineage" id="lineage"><mxGraphModel grid="1" gridSize="10" page="0"><root>\n${cells.join("\n")}\n</root></mxGraphModel></diagram></mxfile>\n`;
}

/** Mermaid flowchart: a subgraph per node, a box per column. */
export function toMermaid(g: LineageGraph, l: Layout = layout(g)): string {
  const q = (s: string) => `"${s.replace(/"/g, "#quot;").replace(/\n/g, " ")}"`;
  const lines = [`flowchart ${l.direction === "tb" ? "TB" : "LR"}`];
  const ids = new Map<string, string>();
  l.nodes.forEach((n, i) => {
    const sid = `N${i}`;
    ids.set(colKey(n.node.id), sid);
    if (n.columns.length === 0) {
      const more = moreLabel(n);
      lines.push(`  ${sid}[${q(`${n.node.name} (${kindLabel(n.node)})${more ? ` · ${more}` : ""}`)}]`);
      return;
    }
    lines.push(`  subgraph ${sid}[${q(`${n.node.name} (${kindLabel(n.node)})`)}]`);
    lines.push("    direction TB");
    n.columns.forEach((c, j) => {
      const cid = `${sid}_${j}`;
      ids.set(colKey(n.node.id, c.name), cid);
      const expr = n.node.columns[c.index]?.expr;
      lines.push(`    ${cid}[${q(expr ? `${c.name} = ${expr}` : c.name)}]`);
    });
    lines.push("  end");
  });
  const styles: string[] = [];
  let k = 0;
  for (const e of shownEdges(g, l)) {
    const s = ids.get(colKey(e.from, e.from_column)) ?? ids.get(colKey(e.from));
    const t = ids.get(colKey(e.to, e.to_column)) ?? ids.get(colKey(e.to));
    if (!s || !t) continue;
    const arrow = e.kind === "filter" || e.kind === "join" ? `-. ${e.kind} .->` : e.kind === "aggregate" ? `==>|aggregate|` : e.kind === "transform" ? `-->|transform|` : "-->";
    lines.push(`  ${s} ${arrow} ${t}`);
    if (e.kind === "filter" || e.kind === "join") styles.push(`  linkStyle ${k} stroke:${LIGHT.warning}`);
    k++;
  }
  return [...lines, ...styles].join("\n") + "\n";
}

/** Graphviz DOT with HTML-like tables (ports per column). */
export function toDot(g: LineageGraph, l: Layout = layout(g)): string {
  const lines = ["digraph lineage {", `  rankdir=${l.direction === "tb" ? "TB" : "LR"};`, '  node [shape=plaintext fontname="Courier"];', '  edge [fontname="Helvetica" fontsize=9];'];
  const port = new Map<string, string>();
  l.nodes.forEach((n, i) => {
    const id = `n${i}`;
    port.set(colKey(n.node.id), id);
    const rows = n.columns.map((c, j) => {
      port.set(colKey(n.node.id, c.name), `${id}:c${j}`);
      const expr = n.node.columns[c.index]?.expr;
      return `<TR><TD PORT="c${j}" ALIGN="LEFT">${xml(expr ? `${c.name} = ${expr}` : c.name)}</TD></TR>`;
    });
    if (moreLabel(n)) rows.push(`<TR><TD ALIGN="LEFT"><FONT COLOR="#64748b">${xml(moreLabel(n))}</FONT></TD></TR>`);
    const border = n.node.kind === "result" || n.node.written ? "#2563eb" : "#94a3b8";
    lines.push(`  ${id} [label=<<TABLE BORDER="1" CELLBORDER="0" CELLSPACING="0" COLOR="${border}"><TR><TD BGCOLOR="#f1f5f9"><B>${xml(n.node.name)}</B></TD></TR>${rows.join("")}</TABLE>>];`);
  });
  for (const e of shownEdges(g, l)) {
    const s = port.get(colKey(e.from, e.from_column)) ?? port.get(colKey(e.from));
    const t = port.get(colKey(e.to, e.to_column)) ?? port.get(colKey(e.to));
    if (!s || !t) continue;
    const st = edgeStyle(e, LIGHT);
    const attrs = [`color="${st.stroke}"`, e.kind !== "direct" ? `label="${e.kind}"` : "", st.dash ? "style=dashed" : "", e.kind === "aggregate" ? "penwidth=2" : ""].filter(Boolean);
    // Ports: right → left (LR), bottom → top (TB).
    const [po, pi] = l.direction === "tb" ? [":s", ":n"] : [":e", ":w"];
    lines.push(`  ${s.includes(":") ? s + po : s} -> ${t.includes(":") ? t + pi : t} [${attrs.join(" ")}];`);
  }
  lines.push("}");
  return lines.join("\n") + "\n";
}

export type ExportFormat = "drawio" | "mermaid" | "dot" | "svg" | "png" | "json";

export const EXPORTS: { format: ExportFormat; label: string; ext: string }[] = [
  { format: "drawio", label: "draw.io (diagrams.net)", ext: "drawio" },
  { format: "mermaid", label: "Mermaid", ext: "mmd" },
  { format: "dot", label: "Graphviz DOT", ext: "dot" },
  { format: "svg", label: "SVG image", ext: "svg" },
  { format: "png", label: "PNG image", ext: "png" },
  { format: "json", label: "JSON (graph data)", ext: "json" },
];

/** Text exports (PNG is rendered from the SVG by the caller). `at` = the layout on screen (direction, moved nodes). */
export function exportText(format: Exclude<ExportFormat, "png">, g: LineageGraph, at: Layout = layout(g)): string {
  const l = normalize(at);
  switch (format) {
    case "drawio":
      return toDrawio(g, l);
    case "mermaid":
      return toMermaid(g, l);
    case "dot":
      return toDot(g, l);
    case "svg":
      return toSvg(g, l);
    case "json":
      return JSON.stringify(g, null, 2) + "\n";
  }
}
