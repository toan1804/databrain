import { describe, expect, it } from "vitest";
import { cellStepBase, stepsFromCells, addDownstream, addEdge, exportFileName, nameProblem, newNode, updateStep, ancestors, descendants, edgeProblem, missingLinks, nodeStatus, parseKeyColumns, changeAction, actionSettings, usesDefaultRows, batchValue, removeNode, scheduleText, uniqueName } from "./jobGraph";
import type { Job, JobNode } from "./types";

const n = (id: string, name: string, sql = "", x = 0, y = 0): JobNode => ({ id, name, kind: "query", sql, x, y, load_mode: "append" });
const job = (nodes: JobNode[], edges: [string, string][] = []): Job => ({
  id: "j",
  name: "J",
  nodes,
  edges: edges.map(([from, to]) => ({ from, to })),
  schedule: { enabled: false, mode: "interval", minutes: 60, at: "08:00", weekdays: [] },
  created_at: 0,
  updated_at: 0,
});

describe("job graph", () => {
  const j = job([n("a", "orders"), n("b", "customers"), n("c", "joined"), n("d", "load")], [["a", "c"], ["b", "c"], ["c", "d"]]);

  it("upstream and downstream, transitively", () => {
    expect([...ancestors(j, "d")].sort()).toEqual(["a", "b", "c"]);
    expect([...descendants(j, "a")].sort()).toEqual(["c", "d"]);
  });

  it("refuses loops, self links and duplicates", () => {
    expect(edgeProblem(j, "d", "a")).toMatch(/loop/);
    expect(edgeProblem(j, "a", "a")).toMatch(/itself/);
    expect(edgeProblem(j, "a", "c")).toMatch(/already/);
    expect(edgeProblem(j, "a", "b")).toBeNull();
    expect(addEdge(j, "d", "a")).toBe(j);
    expect(addEdge(j, "a", "b").edges).toHaveLength(4);
  });

  it("adds a DuckDB downstream step reading the output, or a load step", () => {
    const r = addDownstream(j, "c", "query");
    const added = r.job.nodes.find((x) => x.id === r.id)!;
    expect(added).toMatchObject({ name: "joined_next", connection_id: null, sql: "select *\nfrom results.joined", y: 0 + 76 + 70 });
    expect(added.x).toBe(0);
    // A step already below: the new one goes beside it.
    const busy = { ...j, nodes: j.nodes.map((x) => (x.id === "d" ? { ...x, y: 146 } : x)) };
    const r2 = addDownstream(busy, "c", "query");
    expect(r2.job.nodes.find((x) => x.id === r2.id)!.x).toBe(250);
    expect(r.job.edges).toContainEqual({ from: "c", to: r.id });
    const l = addDownstream(j, "c", "load");
    expect(l.job.nodes.find((x) => x.id === l.id)).toMatchObject({ kind: "load", name: "load_joined", target_table: "joined" });
  });

  it("names, deletes, warnings", () => {
    expect(uniqueName(j.nodes, "orders")).toBe("orders_2");
    expect(uniqueName(j.nodes, "Query 1")).toBe("Query_1");
    expect(uniqueName(j.nodes, "1st")).toBe("_1st");
    const r = removeNode(j, "c");
    expect(r.edges).toEqual([]);
    const w = job([n("a", "orders"), n("b", "x", "select * from results.orders o join results.missing m")]);
    expect(missingLinks(w)).toEqual([{ node: "b", reads: "a" }]);
    expect(missingLinks(addEdge(w, "a", "b"))).toEqual([]);
  });

  it("status and schedule text", () => {
    expect(nodeStatus(n("a", "x"), { id: 1, job_id: "j", trigger: "manual", started_at: 0, status: "running", nodes: [] })?.status).toBe("pending");
    expect(nodeStatus({ ...n("a", "x"), last_run: { status: "success", finished_at: 1, duration_ms: 1, rows: 3 } }, null)?.rows).toBe(3);
    const s = j.schedule;
    expect(scheduleText(s)).toBe("Not scheduled");
    expect(scheduleText({ ...s, enabled: true, minutes: 15 })).toBe("every 15 min");
    expect(scheduleText({ ...s, enabled: true, minutes: 120 })).toBe("every 2 h");
    expect(scheduleText({ ...s, enabled: true, mode: "daily", weekdays: [5, 1] })).toBe("Mon, Fri at 08:00");
    expect(scheduleText({ ...s, enabled: true, mode: "daily" })).toBe("daily at 08:00");
  });

  it("renaming a step renames it in the SQL of steps that read it", () => {
    const k = job([n("a", "orders"), n("b", "x", 'select * from results.orders o join results."orders" p, results.orders_old q'), n("c", "y", "select * from results.orders")], [["a", "b"]]);
    const r = updateStep(k, "a", { name: "sales" });
    expect(r.nodes[0].name).toBe("sales");
    expect(r.nodes[1].sql).toBe("select * from results.sales o join results.sales p, results.orders_old q");
    expect(r.nodes[2].sql).toBe("select * from results.orders"); // not downstream: unchanged
  });

  it("export steps and their file names", () => {
    const e = addDownstream(j, "c", "export");
    expect(e.job.nodes.find((x) => x.id === e.id)).toMatchObject({ kind: "export", name: "export_joined", sql: "", export_format: "csv" });
    const at = new Date(2026, 9, 8, 2, 5, 9);
    expect(exportFileName(null, "sales", "J", "csv", at)).toBe("sales_2026-10-08_020509.csv");
    expect(exportFileName("{job} {date}", "s", "Night/ly", "parquet", at)).toBe("Night_ly 2026-10-08.parquet");
    expect(exportFileName("report.JSON", "s", "j", "json", at)).toBe("report.JSON");
    expect(exportFileName("../x", "s", "j", "csv", at)).toBe("_x.csv");
  });

  it("step names are unique against the job and names used elsewhere", () => {
    expect(uniqueName(j.nodes, "sales", undefined, ["SALES", "sales_2"])).toBe("sales_3");
    expect(uniqueName(j.nodes, "a__b")).toBe("a_b");
    expect(uniqueName(j.nodes, "r12")).toBe("r12_out");
    expect(newNode(j, { name: "orders", x: 0, y: 0 }).name).toBe("orders_2");
    expect(addDownstream(j, "a", "query", {}, ["orders_next"]).job.nodes.at(-1)!.name).toBe("orders_next_2");
    expect(nameProblem("r7")).toMatch(/handles/);
    expect(nameProblem("x__1")).toMatch(/versions/);
    expect(nameProblem("ok_name")).toBeNull();
  });

  it("builds a top-down chain of steps from notebook SQL cells", () => {
    const cells = [
      { source: "select * from orders", output_name: "orders", connection_id: null },
      { source: "  ", output_name: null },
      { source: "select region, sum(x) from results.orders group by 1", output_name: "by_region", connection_id: "duck" },
      { source: "select * from results.by_region", connection_id: "pg2" },
    ];
    const { nodes, edges } = stepsFromCells(cells, ["orders_2", "", "by_region", "sales_4"], "pg", "duck");
    expect(nodes.map((n) => [n.name, n.connection_id, n.sql])).toEqual([
      ["orders_2", "pg", "select * from orders"],
      ["by_region", null, "select region, sum(x) from results.orders_2 group by 1"],
      ["sales_4", "pg2", "select * from results.by_region"],
    ]);
    expect(edges).toEqual([
      { from: nodes[0].id, to: nodes[1].id },
      { from: nodes[1].id, to: nodes[2].id },
    ]);
    expect(nodes.map((n) => n.y)).toEqual([60, 206, 352]);
    expect(cellStepBase({ source: "x" }, "Sales", 3)).toBe("sales_3");
  });
});

describe("parseKeyColumns", () => {
  it("splits on commas and drops empty items", () => {
    expect(parseKeyColumns(" id, region ,, ")).toEqual(["id", "region"]);
    expect(parseKeyColumns("")).toEqual([]);
  });
});

describe("one action per step", () => {
  const base: JobNode = { id: "x", name: "x", kind: "load", sql: "", x: 0, y: 0, load_mode: "merge", target_connection_id: "pg", target_table: "t", key_columns: ["id"], load_before_sql: "delete from t" };
  it("lists what a change of action removes", () => {
    expect(actionSettings(base)).toEqual(["the target connection and table", "the before/after SQL", "the key columns"]);
    expect(actionSettings({ ...base, kind: "query", sql: " " })).toEqual([]);
  });
  it("clears the other actions' settings", () => {
    const q = { ...base, ...changeAction(base, "query", ["orders"]) };
    expect(q).toMatchObject({ kind: "query", target_connection_id: null, target_table: null, load_before_sql: null, key_columns: [], load_mode: "append", sql: "select *\nfrom results.orders" });
    const e = { ...q, ...changeAction(q, "export", ["orders"]) };
    expect(e).toMatchObject({ kind: "export", sql: "", connection_id: null });
    expect(changeAction(e, "load", ["orders"])).toMatchObject({ kind: "load", target_table: "orders" });
    expect(changeAction(e, "export", [])).toEqual({});
  });
  it("knows when a step reads all upstream rows", () => {
    expect(usesDefaultRows({ ...base, sql: "" }, [])).toBe(true);
    expect(usesDefaultRows({ ...base, sql: "SELECT *\n FROM results.Orders" }, ["orders"])).toBe(true);
    expect(usesDefaultRows({ ...base, sql: "select id from results.orders" }, ["orders"])).toBe(false);
  });
});

describe("batchValue", () => {
  it("keeps whole numbers within the maximum; empty is the default", () => {
    expect(batchValue("", 1000)).toBeNull();
    expect(batchValue("0", 1000)).toBeNull();
    expect(batchValue("abc", 1000)).toBeNull();
    expect(batchValue("250.7", 1000)).toBe(250);
    expect(batchValue("5000", 1000)).toBe(1000);
  });
});
