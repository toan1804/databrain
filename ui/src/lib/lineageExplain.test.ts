import { describe, expect, it } from "vitest";
import { explainColumn } from "./lineageExplain";
import type { LineageGraph } from "./types";

const at = (sql: string, s: string, from = 0) => {
  const i = sql.indexOf(s, from);
  return { start: i, end: i + s.length };
};

const sql =
  "with paid as (select * from orders where status = 'paid'), " +
  "net as (select p.customer_id, p.amount * p.rate as net_amount from paid p) " +
  "select c.name, sum(n.net_amount) as total from net n left join customers c on c.id = n.customer_id group by 1";

const g: LineageGraph = {
  nodes: [
    { id: "o", kind: "table", name: "sales.orders", columns: ["customer_id", "amount", "rate", "status"].map((name) => ({ name })), columns_known: true, written: false },
    { id: "p", kind: "cte", name: "paid", columns: ["customer_id", "amount", "rate", "status"].map((name) => ({ name })), columns_known: true, written: false },
    { id: "n", kind: "cte", name: "net", columns: [{ name: "customer_id" }, { name: "net_amount", expr: "p.amount * p.rate" }], columns_known: true, written: false },
    { id: "c", kind: "table", name: "sales.customers", columns: [{ name: "id" }, { name: "name" }], columns_known: true, written: false },
    { id: "r", kind: "result", name: "Result", columns: [{ name: "name" }, { name: "total", expr: "sum(n.net_amount)" }], columns_known: true, written: false },
  ],
  edges: [
    ...["customer_id", "amount", "rate", "status"].map((c) => ({ from: "o", from_column: c, to: "p", to_column: c, kind: "direct" as const })),
    { from: "o", from_column: "status", to: "p", kind: "filter", detail: "WHERE status = 'paid'" },
    { from: "p", from_column: "customer_id", to: "n", to_column: "customer_id", kind: "direct", span: at(sql, "p.customer_id") },
    { from: "p", from_column: "amount", to: "n", to_column: "net_amount", kind: "transform", span: at(sql, "p.amount") },
    { from: "p", from_column: "rate", to: "n", to_column: "net_amount", kind: "transform", span: at(sql, "p.rate") },
    { from: "n", from_column: "net_amount", to: "r", to_column: "total", kind: "aggregate", span: at(sql, "n.net_amount") },
    { from: "c", from_column: "name", to: "r", to_column: "name", kind: "direct" },
    { from: "c", from_column: "id", to: "r", kind: "join", detail: "LEFT JOIN customers c ON c.id = n.customer_id" },
    { from: "n", from_column: "customer_id", to: "r", kind: "join", detail: "LEFT JOIN customers c ON c.id = n.customer_id" },
  ],
  statements: [],
  warnings: [],
  partial: false,
  unknown_tables: [],
};

describe("explainColumn", () => {
  it("one formula in database columns, then the steps from source to result", () => {
    const x = explainColumn(g, sql, "r", "total")!;
    expect(x.formula).toBe("sum(sales.orders.amount * sales.orders.rate)");
    expect(x.sources.sort()).toEqual(["sales.orders.amount", "sales.orders.rate"]);
    expect(x.rowColumns.sort()).toEqual(["net.customer_id", "sales.customers.id", "sales.orders.status"]);
    const steps = x.steps.map((s) => [
      s.node.name,
      s.computed.map((c) => `${c.name} = ${c.expr}`).join(", "),
      s.passed.map((c) => c.name).join(", "),
      s.conditions.map((c) => c.detail).join(" | "),
    ]);
    expect(steps).toEqual([
      ["sales.orders", "", "amount, rate", ""],
      ["paid", "", "amount, rate", "WHERE status = 'paid'"],
      ["net", "net_amount = p.amount * p.rate", "", ""],
      ["Result", "total = sum(n.net_amount)", "", "LEFT JOIN customers c ON c.id = n.customer_id"],
    ]);
  });

  it("copied columns read as their source; pass-through steps without conditions fold away", () => {
    const x = explainColumn(g, sql, "r", "name")!;
    expect(x.formula).toBe("sales.customers.name");
    expect(x.steps.map((s) => s.node.name)).toEqual(["sales.customers", "Result"]);
    const noFilter = { ...g, edges: g.edges.filter((e) => e.kind !== "filter") };
    expect(explainColumn(noFilter, sql, "r", "total")!.steps.map((s) => s.node.name)).toEqual(["sales.orders", "net", "Result"]);
  });

  it("doesn't replace inside longer names", () => {
    const g2: LineageGraph = {
      ...g,
      nodes: [g.nodes[0], { id: "r", kind: "result", name: "Result", columns: [{ name: "x", expr: "amount + amount_2" }], columns_known: true, written: false }],
      edges: [{ from: "o", from_column: "amount", to: "r", to_column: "x", kind: "transform", span: { start: 0, end: 6 } }],
    };
    expect(explainColumn(g2, "amount + amount_2", "r", "x")!.formula).toBe("sales.orders.amount + amount_2");
  });
});

import { columnFlow, conditionText, type FlowNode } from "./lineageExplain";

describe("columnFlow", () => {
  const line = (f: FlowNode, d = 0): string[] => [
    `${"  ".repeat(d)}${f.node.name}.${f.column}${f.through.length ? ` (via ${f.through.join(", ")})` : ""}${f.repeat ? " (above)" : ""}`,
    ...(f.children.length ? [`${"  ".repeat(d)}  ↓ ${f.expr ?? "copied"}${f.conditions.map((c) => ` | ${conditionText(c)}`).join("")}`] : []),
    ...f.children.flatMap((c) => line(c, d + 1)),
  ];
  it("top: the column; arrows: expressions and joins; below: inputs side by side", () => {
    expect(line(columnFlow(g, "r", "total")!)).toEqual([
      "Result.total",
      "  ↓ sum(n.net_amount) | left join customers c on c.id = n.customer_id",
      "  net.net_amount",
      "    ↓ p.amount * p.rate",
      "    paid.amount",
      "      ↓ copied | where status = 'paid'",
      "      sales.orders.amount",
      "    paid.rate",
      "      ↓ copied",
      "      sales.orders.rate",
    ]);
  });
  it("folds plain copies into the arrow", () => {
    const noFilter = { ...g, edges: g.edges.filter((e) => e.kind !== "filter") };
    const f = columnFlow(noFilter, "n", "net_amount")!;
    expect(f.children.map((c) => `${c.node.name}.${c.column} via ${c.through.join(",")}`)).toEqual(["sales.orders.amount via paid", "sales.orders.rate via paid"]);
  });
});
