import { describe, expect, it } from "vitest";
import { layout, trace, colKey, anchor, withPositions, normalize, shownEdges } from "./lineageLayout";
import { exportText, toDrawio, toMermaid, toDot, toSvg } from "./lineageExport";
import type { LineageGraph } from "./types";

// What the backend returns for:
//   with paid as (select * from orders where status = 'paid')
//   select c.name, sum(p.amount) total from paid p join customers c on c.id = p.customer_id group by 1
const g: LineageGraph = {
  nodes: [
    { id: "n0", kind: "table", name: "sales.orders", columns: ["id", "customer_id", "amount", "status"].map((name) => ({ name })), columns_known: true, written: false },
    { id: "n1", kind: "cte", name: "paid", columns: ["id", "customer_id", "amount", "status"].map((name) => ({ name })), columns_known: true, written: false, statement: 0 },
    { id: "n2", kind: "table", name: 'sales."Customers" <&>', columns: [{ name: "id" }, { name: "name" }], columns_known: true, written: false },
    { id: "n3", kind: "result", name: "Result", columns: [{ name: "name" }, { name: "total", expr: 'sum(p.amount) "x"' }], columns_known: true, written: false, statement: 0 },
  ],
  edges: [
    ...["id", "customer_id", "amount", "status"].map((c) => ({ from: "n0", from_column: c, to: "n1", to_column: c, kind: "direct" as const })),
    { from: "n0", from_column: "status", to: "n1", kind: "filter" },
    { from: "n2", from_column: "name", to: "n3", to_column: "name", kind: "direct" },
    { from: "n1", from_column: "amount", to: "n3", to_column: "total", kind: "aggregate" },
    { from: "n2", from_column: "id", to: "n3", kind: "join", detail: "LEFT JOIN customers c ON c.id = p.customer_id" },
    { from: "n1", from_column: "customer_id", to: "n3", kind: "join", detail: "LEFT JOIN customers c ON c.id = p.customer_id" },
  ],
  statements: [{ index: 0, kind: "select", range: { start: 0, end: 10 } }],
  warnings: [],
  partial: false,
  unknown_tables: [],
};

describe("lineage layout", () => {
  it("places sources left of what they feed, no overlaps", () => {
    const l = layout(g);
    const x = (id: string) => l.byId.get(id)!.x;
    expect(x("n0")).toBeLessThan(x("n1"));
    expect(x("n1")).toBeLessThan(x("n3"));
    expect(x("n2")).toBe(x("n0"));
    const [a, b] = [l.byId.get("n0")!, l.byId.get("n2")!];
    expect(a.y + a.h <= b.y || b.y + b.h <= a.y).toBe(true);
    expect(anchor(l, "n3", "total", "in")!.y).toBeGreaterThan(anchor(l, "n3", "name", "in")!.y);
    expect(anchor(l, "n3", undefined, "in")!.y).toBeLessThan(anchor(l, "n3", "name", "in")!.y);
  });

  it("folds unused columns of wide tables", () => {
    const wide: LineageGraph = {
      ...g,
      nodes: [{ ...g.nodes[0], columns: Array.from({ length: 40 }, (_, i) => ({ name: i === 7 ? "amount" : `c${i}` })) }, g.nodes[3]],
      edges: [{ from: "n0", from_column: "amount", to: "n3", to_column: "total", kind: "aggregate" }],
    };
    const n = layout(wide).byId.get("n0")!;
    expect(n.columns.map((c) => c.name)).toEqual(["amount"]);
    expect(n.hidden).toBe(39);
  });

  it("traces a column up to its sources and filters, and down", () => {
    const t = trace(g, "n3", "total");
    expect([...t.cols].sort()).toEqual([colKey("n0", "amount"), colKey("n0", "status"), colKey("n1", "amount"), colKey("n1", "customer_id"), colKey("n2", "id"), colKey("n3", "total")].sort());
    const d = trace(g, "n0", "amount");
    expect(d.cols.has(colKey("n3", "total"))).toBe(true);
    expect(d.cols.has(colKey("n3", "name"))).toBe(false);
  });
});

describe("table-level first", () => {
  const none = { expanded: new Set<string>() };
  it("collapsed tables list no columns and parallel edges merge into one table edge", () => {
    const l = layout(g, "lr", none);
    expect(l.nodes.every((n) => n.collapsed && n.columns.length === 0)).toBe(true);
    const e = shownEdges(g, l);
    const key = (x: (typeof e)[number]) => `${x.from}>${x.to}:${x.kind}`;
    // orders→paid: 4 copied columns + a filter = 2 lines; paid→Result: aggregate + join.
    expect(e.map(key).sort()).toEqual(["n0>n1:direct", "n0>n1:filter", "n1>n3:aggregate", "n1>n3:join", "n2>n3:direct", "n2>n3:join"]);
    expect(e.find((x) => key(x) === "n0>n1:direct")!.indexes).toHaveLength(4);
    expect(l.byId.get("n0")!.h).toBeLessThan(layout(g).byId.get("n0")!.h);
    const svg = toSvg(g, l, { inner: true });
    expect(svg).toContain("▸ sales.orders");
    expect(svg).toContain("4 columns · click to show");
    expect(toMermaid(g, l)).toContain('N0["sales.orders (table) · 4 columns"]');
  });

  it("an expanded table lists its columns; a traced column shows its path in collapsed ones", () => {
    const l = layout(g, "lr", { expanded: new Set(["n3"]) });
    expect(l.byId.get("n3")!.columns.map((c) => c.name)).toEqual(["name", "total"]);
    expect(l.byId.get("n1")!.columns).toEqual([]);
    const t = trace(g, "n3", "total");
    const lt = layout(g, "lr", { expanded: new Set(["n3"]), show: t.cols });
    expect(lt.byId.get("n1")!.columns.map((c) => c.name)).toEqual(["customer_id", "amount"]);
    expect(lt.byId.get("n0")!.columns.map((c) => c.name)).toEqual(["amount", "status"]);
    expect(lt.byId.get("n2")!.columns.map((c) => c.name)).toEqual(["id"]);
    expect(moreText(lt, "n0")).toBe("+2 more columns");
    // The traced path is drawn column to column.
    expect(shownEdges(g, lt).some((e) => e.from === "n1" && e.from_column === "amount" && e.to_column === "total")).toBe(true);
  });
});

function moreText(l: ReturnType<typeof layout>, id: string): string {
  const svg = toSvg(g, { ...l, nodes: [l.byId.get(id)!] }, { inner: true });
  return svg.match(/>(\+\d+ more columns?)[ <]/)?.[1] ?? "";
}

describe("lineage direction and moved tables", () => {
  it("top to bottom: sources above, a rank side by side", () => {
    const l = layout(g, "tb");
    const n = (id: string) => l.byId.get(id)!;
    expect(n("n0").y + n("n0").h).toBeLessThan(n("n1").y);
    expect(n("n1").y + n("n1").h).toBeLessThan(n("n3").y);
    expect(n("n2").y).toBe(n("n0").y);
    expect(n("n0").x + n("n0").w <= n("n2").x || n("n2").x + n("n2").w <= n("n0").x).toBe(true);
    expect(l.direction).toBe("tb");
    expect(toMermaid(g, l).startsWith("flowchart TB\n")).toBe(true);
    expect(toDot(g, l)).toContain("rankdir=TB");
    expect(toDot(g, l)).toMatch(/n1:c2:s -> n3:c1:n/);
    // Lines leave the bottom of the source and enter the top of the target.
    const out = anchor(l, "n1", "amount", "out")!;
    const inn = anchor(l, "n3", "total", "in")!;
    expect(out.y).toBe(n("n1").y + n("n1").h);
    expect(inn.y).toBe(n("n3").y);
    expect(out.x).toBeGreaterThan(n("n1").x);
    expect(out.x).toBeLessThan(n("n1").x + n("n1").w);
    expect(anchor(l, "n1", "id", "out")!.x).toBeLessThan(out.x);
    expect(anchor(l, "n1", undefined, "out")!.x).toBe(n("n1").x + n("n1").w / 2);
    expect(toDrawio(g, l)).toContain("exitY=1;entryX=0.5;entryY=0");
  });

  it("moved tables carry their columns and edges; exports start at the margin", () => {
    const l = layout(g);
    const moved = withPositions(l, { n3: { x: -200, y: 500 } });
    expect(anchor(moved, "n3", "total", "in")!.y - anchor(l, "n3", "total", "in")!.y).toBe(500 - l.byId.get("n3")!.y);
    expect(l.byId.get("n3")!.x).not.toBe(-200);
    expect(withPositions(l, {})).toBe(l);
    const n = normalize(moved);
    expect(Math.min(...n.nodes.map((x) => x.x))).toBe(24);
    expect(exportText("drawio", g, moved)).toMatch(/<mxGeometry x="24" y="(524|\d+)"/);
  });
});

describe("lineage exports", () => {
  it("mermaid: subgraphs, column boxes, labelled edges, escaped quotes", () => {
    const m = toMermaid(g);
    expect(m.startsWith("flowchart LR\n")).toBe(true);
    expect(m).toContain('subgraph N0["sales.orders (table)"]');
    expect(m).toContain('["total = sum(p.amount) #quot;x#quot;"]');
    expect(m).toMatch(/N1_2 ==>\|aggregate\| N3_1/);
    expect(m).toMatch(/N2_0 -\. join \.-> N3\n/);
    expect(m).not.toMatch(/[^#]quot/);
  });

  it("draw.io: containers with column rows and edges between them", () => {
    const x = toDrawio(g);
    expect(x).toMatch(/^<mxfile host="DataBrain"><diagram name="Lineage"/);
    expect((x.match(/swimlane/g) ?? []).length).toBe(4);
    expect(x).toContain('value="sales.&quot;Customers&quot; &lt;&amp;&gt;"');
    expect(x).toMatch(/edge="1" parent="1" source="node1c2" target="node3c1"/);
    expect(x).toMatch(/source="node2c0" target="node3"/);
  });

  it("dot and svg", async () => {
    const d = toDot(g);
    expect(d).toContain("rankdir=LR");
    expect(d).toMatch(/n1:c2:e -> n3:c1:w \[color="[^"]+" label="aggregate" penwidth=2\];/);
    const s = toSvg(g);
    expect(s.startsWith('<svg xmlns="http://www.w3.org/2000/svg"')).toBe(true);
    expect(s).toContain('data-col="total"');
    expect(JSON.parse(exportText("json", g)).nodes).toHaveLength(4);
    // Well-formedness of the XML files is checked with xmllint when asked.
    const dir = (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env.LINEAGE_EXPORT_DIR;
    if (dir) {
      const fsName = "node:fs";
      const fs = (await import(/* @vite-ignore */ fsName)) as { writeFileSync: (p: string, d: string) => void };
      fs.writeFileSync(`${dir}/t.drawio`, toDrawio(g));
      fs.writeFileSync(`${dir}/t.svg`, s);
    }
  });
});
