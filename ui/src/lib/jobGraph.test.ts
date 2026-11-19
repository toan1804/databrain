import { describe, expect, it } from "vitest";
import { cellStepBase, stepsFromCells, addDownstream, addEdge, exportFileName, nameProblem, newNode, updateStep, ancestors, descendants, cappedReads, edgeProblem, missingLinks, nodeStatus, parseKeyColumns, changeAction, actionSettings, usesDefaultRows, batchValue, removeNode, scheduleText, uniqueName, browserSignInConnections, monthDayText, scheduleTimes, toggleMonthDay, triggerText, cronProblem, timeZoneProblem, afterLoopJobs, sameSchedule, rebaseClashes, rebaseDeleted, rebaseJob } from "./jobGraph";
import type { Job, JobNode, OutputInfo } from "./types";

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

describe("capped outputs a step reads", () => {
  const out = (name: string | null, handle: string, truncated: boolean, rows = 1000) => ({ name, handle, truncated, rows }) as OutputInfo;
  const outputs = [out("top", "r1", true), out("full", "r2", false), out(null, "r3", true, 500)];
  it("warns DuckDB steps reading a capped output from outside the job, by name or handle", () => {
    const j = job([n("a", "first", "select * from results.top join results.R3 using (id)"), n("b", "second", "select * from results.full")]);
    expect(cappedReads(j, outputs)).toEqual([
      { node: "a", reads: "top", rows: 1000 },
      { node: "a", reads: "R3", rows: 500 },
    ]);
  });
  it("not a job step of the same name, nor a query on another database", () => {
    const j = job([n("a", "top"), n("b", "next", "select * from results.top"), { ...n("c", "remote", "select * from results.top"), connection_id: "pg" }]);
    expect(cappedReads(j, outputs)).toEqual([]);
    const k = job([{ ...n("c", "remote", "select * from results.full, results.r1"), connection_id: "pg" }, { ...n("d", "dl", "select * from results.r1"), kind: "load" }]);
    expect(cappedReads(k, outputs)).toEqual([{ node: "d", reads: "r1", rows: 1000 }]);
    expect(cappedReads(k, outputs, "pg").map((w) => w.node)).toEqual(["c", "d"]);
  });
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
    // A taken name ending in `_` gets a suffix the editor accepts, never `__`.
    for (const base of ["sales_", "sales__", "Sales!", `${"a".repeat(55)}_x`]) {
      const name = uniqueName([], base, undefined, [uniqueName([], base)]);
      expect(nameProblem(name), `${base} → ${name}`).toBeNull();
      expect(name.includes("__")).toBe(false);
    }
    expect(uniqueName([], "sales_", undefined, ["sales_"])).toBe("sales_2");
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

describe("job steps read outputs as the shared scanner does", () => {
  it("doesn't take results.x in a string or comment for a link, and sees one behind a comment", () => {
    const k = job([n("a", "orders"), n("b", "x", "select 'results.orders' as label -- results.orders"), n("c", "y", "select * from results/**/.orders")]);
    expect(missingLinks(k)).toEqual([{ node: "c", reads: "a" }]);
  });
});

describe("schedule editor", () => {
  const s = job([]).schedule;
  it("describes every kind of schedule, its time zone and the jobs it runs after", () => {
    expect(scheduleText({ ...s, enabled: true, mode: "daily", times: ["18:00", "06:30"], weekdays: [1, 2, 3, 4, 5] })).toBe("Mon, Tue, Wed, Thu, Fri at 06:30, 18:00");
    expect(scheduleText({ ...s, enabled: true, mode: "monthly", month_days: [-1, 15, 1], at: "05:00" })).toBe("monthly on the 1st, 15th, last day at 05:00");
    expect(scheduleText({ ...s, enabled: true, mode: "monthly" })).toBe("monthly on the 1st at 08:00");
    expect(scheduleText({ ...s, enabled: true, mode: "cron", cron: "30 6 * * 1-5", time_zone: "Europe/Paris" })).toBe("cron 30 6 * * 1-5 (Europe/Paris)");
    const names = (id: string) => ({ a: "Extract", b: "Clean" })[id];
    expect(scheduleText({ ...s, after_jobs: ["a", "b"] }, names)).toBe("after Extract or Clean");
    expect(scheduleText({ ...s, after_jobs: ["gone"] }, names)).toBe("after another job");
    expect(scheduleText({ ...s, enabled: true, minutes: 60, after_jobs: ["a"] }, names)).toBe("every 1 h, and after Extract");
  });
  it("times of day and days of the month", () => {
    expect(scheduleTimes(s)).toEqual(["08:00"]);
    expect(scheduleTimes({ ...s, times: ["18:00", "06:30", "18:00"] })).toEqual(["06:30", "18:00"]);
    expect(toggleMonthDay([], 15)).toEqual([1, 15]);
    expect(toggleMonthDay([1, 15], 1)).toEqual([15]);
    expect(toggleMonthDay([15], 15)).toEqual([15]); // one stays on
    expect(toggleMonthDay([-1, 31], 2)).toEqual([2, 31, -1]);
    expect([1, 2, 3, 11, 12, 13, 21, 22, 23, 31, -1].map(monthDayText)).toEqual(["1st", "2nd", "3rd", "11th", "12th", "13th", "21st", "22nd", "23rd", "31st", "last day"]);
  });
  it("names connections with browser sign-in that the job's steps use", () => {
    const k = job([{ ...n("a", "x"), connection_id: "sf" }, { ...n("b", "y"), kind: "load", target_connection_id: "pg" }, { ...n("c", "z"), connection_id: "bq" }]);
    const conns = [
      { id: "sf", name: "Snowflake SSO", config: { auth: { method: "external_browser" } } },
      { id: "pg", name: "Warehouse", config: { auth: { method: "oauth_browser" } } },
      { id: "bq", name: "BigQuery", config: { auth: { method: "service_account" } } },
      { id: "other", name: "Unused", config: { auth: { method: "device_code" } } },
    ];
    expect(browserSignInConnections(k, conns)).toEqual(["Snowflake SSO", "Warehouse"]);
  });
  it("run triggers", () => {
    expect(triggerText("schedule")).toBe("scheduled");
    expect(triggerText("after:Extract")).toBe("after Extract");
    expect(triggerText("cli")).toBe("command line");
  });
});

describe("schedule checks at the field (the scheduler's rules)", () => {
  it("accepts what the scheduler accepts", () => {
    for (const ok of ["30 6 * * 1-5", "*/15 * * * *", "0 0 1 * *", "@daily", "@Weekly", "0 9 * JAN-MAR mon,wed,FRI", "5/15 * * * *", "0 0 * * 7", "0 0 31 * *", "0 0 29 2 *", "0 0 30 2 1", "1-59/4294967295 * * * *"]) {
      expect(cronProblem(ok), ok).toBeNull();
    }
    for (const tz of ["", "local", "UTC", "gmt", "Europe/Paris", "America/New_York", "+05:30", "-08:00", "UTC+2", "+14:00"]) expect(timeZoneProblem(tz), tz).toBeNull();
    // As the scheduler reads them: the UTC/GMT prefix in any case, day names going round the week.
    for (const tz of ["utc+2", "gmt-3", "Utc+05:30"]) expect(timeZoneProblem(tz), tz).toBeNull();
    for (const ok of ["0 9 * * fri-sun", "0 9 * * sat-mon", "0 9 * * fri-tue/2", "0 0 29 2 *", "0 0 13 * fri", "0 12 13 * */7"]) expect(cronProblem(ok), ok).toBeNull();
    expect(cronProblem("0 9 * * 5-1")).toContain("backwards");
    // Too rare for the scheduler's 8-year look ahead.
    for (const rare of ["0 0 29 2 */7", "0 0 25 12 */7", "0 0 1 2 */7"]) expect(cronProblem(rare), rare).toContain("too rarely");
  });

  it("refuses what it refuses", () => {
    for (const bad of ["", "* * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "* * * 13 *", "* * * * 8", "5-1 * * * *", "*/0 * * * *", "x * * * *", "0 0 30 2 *", "0 0 31 4,6 *"]) {
      expect(cronProblem(bad), bad).not.toBeNull();
    }
    for (const tz of ["Europe/Pari", "+-5:00", "+5:-30", "+15:00", "+05:60", "Mars/Olympus"]) expect(timeZoneProblem(tz), tz).not.toBeNull();
  });

  it("finds the jobs that would loop, and compares schedules by what runs", () => {
    const j = (id: string, after: string[] = []) => ({ id, schedule: { after_jobs: after } });
    expect([...afterLoopJobs("a", [j("a"), j("b", ["a"]), j("c", ["b"]), j("d"), j("e", ["d", "c"])])].sort()).toEqual(["a", "b", "c", "e"]);
    const s = { enabled: true, mode: "daily", minutes: 60, at: "06:00", weekdays: [1, 2] } as Job["schedule"];
    expect(sameSchedule(s, { ...s, times: ["06:00"], weekdays: [2, 1], missed: "once" })).toBe(true);
    expect(sameSchedule(s, { ...s, at: "07:00" })).toBe(false);
    expect(sameSchedule(s, undefined)).toBe(false);
  });
});

describe("rebaseJob: edits made here put on top of the version saved since", () => {
  const node = (id: string, extra: Partial<JobNode> = {}): JobNode => ({ id, name: id, kind: "query", sql: `select '${id}'`, x: 0, y: 0, load_mode: "append", ...extra });
  const job = (nodes: JobNode[], extra: Partial<Job> = {}): Job => ({
    id: "J",
    name: "J",
    nodes,
    edges: [],
    schedule: { enabled: false, mode: "interval", minutes: 60, at: "08:00", weekdays: [] },
    created_at: 1,
    updated_at: 1,
    ...extra,
  });

  it("a step deleted since whose SQL the user changed is kept as the user's, with its links; other edits to it are told, not kept", () => {
    const base = job([node("a"), node("b", { sql: "select 1", ai_sql: true }), node("c", { ai_sql: true })], { edges: [{ from: "a", to: "b" }, { from: "b", to: "c" }] });
    const mine = job([node("a"), node("b", { sql: "select 10", ai_sql: true }), node("c", { ai_sql: true, name: "renamed", x: 50 })], { edges: [{ from: "a", to: "b" }, { from: "b", to: "c" }] });
    // The assistant deleted b and c.
    const theirs = job([node("a")], { updated_at: 2 });
    const out = rebaseJob(base, mine, theirs);
    expect(out.nodes.map((n) => n.id)).toEqual(["a", "b"]);
    expect(out.nodes[1]).toMatchObject({ sql: "select 10", ai_sql: false });
    expect(out.edges).toEqual([{ from: "a", to: "b" }]);
    const d = rebaseDeleted(base, mine, theirs);
    expect(d.kept.map((n) => n.id)).toEqual(["b"]);
    expect(d.dropped.map((n) => n.id)).toEqual(["c"]);
    // A step only moved, or left alone, goes quietly with the deletion.
    const moved = job([node("a"), node("b", { sql: "select 1", ai_sql: true, x: 99 }), node("c", { ai_sql: true })], { edges: base.edges });
    expect(rebaseDeleted(base, moved, theirs)).toEqual({ kept: [], replaced: [], dropped: [] });
    expect(rebaseJob(base, moved, theirs).nodes.map((n) => n.id)).toEqual(["a"]);
  });

  it("a step replaced by a new one of its name keeps the new one (told), a kept step's links can't close a loop, other edits aren't kept", () => {
    const base = job([node("a"), node("b", { name: "orders", sql: "select 1", ai_sql: true }), node("c")], { edges: [{ from: "a", to: "b" }, { from: "b", to: "c" }] });
    const mine = job([node("a"), node("b", { name: "orders", sql: "select 10", ai_sql: true }), node("c", { sql: "select * from results.orders", ai_sql: true })], { edges: base.edges });
    // The assistant replaced b with z, a new step of the same name that c now follows.
    const theirs = job([node("a"), node("z", { name: "orders", ai_sql: true }), node("c", { sql: "select * from results.orders", ai_sql: true })], { updated_at: 2, edges: [{ from: "z", to: "c" }] });
    const d = rebaseDeleted(base, mine, theirs);
    expect(d.replaced.map((n) => n.id)).toEqual(["b"]);
    expect(d.kept).toEqual([]);
    const out = rebaseJob(base, mine, theirs);
    expect(out.nodes.map((n) => n.id)).toEqual(["a", "z", "c"]);
    expect(out.nodes[2]).toMatchObject({ sql: "select * from results.orders", ai_sql: true });
    expect(out.edges).toEqual([{ from: "z", to: "c" }]);

    // Kept (its name is free): its links come back, but not one that closes a loop with a link added since.
    const looped = job([node("a"), node("c")], { updated_at: 2, edges: [{ from: "c", to: "a" }] });
    const kept = rebaseJob(base, job([node("a"), node("b", { name: "orders", sql: "select 10" }), node("c"), node("d")], { edges: [...base.edges, { from: "d", to: "b" }] }), looped);
    expect(kept.nodes.find((n) => n.id === "b")).toMatchObject({ name: "orders", ai_sql: false });
    expect(kept.edges).toContainEqual({ from: "c", to: "a" });
    expect(kept.edges).toContainEqual({ from: "d", to: "b" });
    expect(kept.edges.some((e) => e.from === "a" && e.to === "b") && kept.edges.some((e) => e.from === "b" && e.to === "c")).toBe(false);

    // Only before/after SQL changed: dropped too (the step's SQL is the assistant's).
    const after = job([node("a"), node("b", { name: "orders", sql: "select 1", ai_sql: true, load_after_sql: "analyze t" }), node("c")], { edges: base.edges });
    expect(rebaseDeleted(base, after, theirs).dropped.map((n) => n.id)).toEqual(["b"]);

    // Only the connection changed: the assistant's SQL isn't made the user's.
    const conn = job([node("a"), node("b", { name: "orders", sql: "select 1", ai_sql: true, connection_id: "other" }), node("c")], { edges: base.edges });
    expect(rebaseDeleted(base, conn, theirs).kept).toEqual([]);
    expect(rebaseDeleted(base, conn, theirs).dropped.map((n) => n.id)).toEqual(["b"]);
    expect(rebaseJob(base, conn, theirs).nodes.some((n) => n.id === "b")).toBe(false);

    // Deleted here too: nothing to keep or say.
    const gone = job([node("a"), node("c")]);
    expect(rebaseDeleted(base, gone, theirs)).toEqual({ kept: [], replaced: [], dropped: [] });
    // A link from a kept step to a step deleted on both sides is dropped.
    const both = job([node("a"), node("b", { name: "orders", sql: "select 10" })], { edges: [{ from: "b", to: "c" }] });
    const noC = job([node("a")], { updated_at: 2 });
    expect(rebaseJob(base, both, noC).edges).toEqual([]);
  });

  it("keeps the assistant's newer SQL (and its mark) under a step the user only moved or renamed", () => {
    const base = job([node("a", { sql: "select 1", ai_sql: true })]);
    const mine = job([node("a", { sql: "select 1", ai_sql: true, x: 200, name: "renamed" })]);
    const theirs = job([node("a", { sql: "select 2", ai_sql: true })], { updated_at: 2 });
    const out = rebaseJob(base, mine, theirs);
    expect(out.nodes[0]).toMatchObject({ sql: "select 2", ai_sql: true, x: 200, name: "renamed" });
    expect(out.updated_at).toBe(2);
  });

  it("keeps the user's own SQL edit whole when the step wasn't changed since (the step is then the user's to save)", () => {
    const base = job([node("a", { sql: "select 1", load_before_sql: "delete from t", ai_sql: true })]);
    const mine = job([node("a", { sql: "select 10", load_before_sql: "delete from t", ai_sql: true })]);
    const theirs = job([node("a", { sql: "select 1", load_before_sql: "delete from t", ai_sql: true, x: 9 })], { updated_at: 2 });
    const out = rebaseJob(base, mine, theirs);
    expect(out.nodes[0]).toMatchObject({ sql: "select 10", load_before_sql: "delete from t", x: 9 });
    expect(rebaseClashes(base, mine, theirs)).toEqual([]);
  });

  it("a step both changed keeps the version since, mark included (never the user's SQL with the assistant's newer before-SQL or target)", () => {
    const base = job([node("a", { kind: "load", sql: "select 1", load_before_sql: "delete from t", target_table: "t", ai_sql: false })]);
    const mine = job([node("a", { kind: "load", sql: "select 10", load_before_sql: "delete from t", target_table: "t", ai_sql: false, x: 7 })]);
    for (const since of [
      { sql: "select 1", load_before_sql: "drop table t", target_table: "t" },
      { sql: "select 1", load_before_sql: "delete from t", target_table: "other" },
      { sql: "select 1", load_before_sql: "delete from t", target_table: "t", load_mode: "replace" as const },
    ]) {
      const theirs = job([node("a", { kind: "load", ...since, ai_sql: true })], { updated_at: 2 });
      const out = rebaseJob(base, mine, theirs);
      expect(out.nodes[0]).toMatchObject({ ...since, ai_sql: true, x: 7 });
      expect(rebaseClashes(base, mine, theirs)).toEqual(["a"]);
    }
  });

  it("'Trust this SQL' counts only for the SQL the user trusted", () => {
    const base = job([node("a", { sql: "select 1", ai_sql: true })]);
    const mine = job([node("a", { sql: "select 1", ai_sql: false })]);
    const changed = rebaseJob(base, mine, job([node("a", { sql: "select 2", ai_sql: true })], { updated_at: 2 }));
    expect(changed.nodes[0]).toMatchObject({ sql: "select 2", ai_sql: true });
    const same = rebaseJob(base, mine, job([node("a", { sql: "select 1", ai_sql: true, x: 5 })], { updated_at: 2 }));
    expect(same.nodes[0]).toMatchObject({ sql: "select 1", ai_sql: false, x: 5 });
  });

  it("adds and removes steps and links from both sides", () => {
    const base = job([node("a"), node("b")], { edges: [{ from: "a", to: "b" }] });
    const mine = job([node("a"), node("c")], { edges: [{ from: "a", to: "c" }] });
    const theirs = job([node("a"), node("b"), node("d")], { edges: [{ from: "a", to: "b" }, { from: "a", to: "d" }], updated_at: 2 });
    const out = rebaseJob(base, mine, theirs);
    expect(out.nodes.map((n) => n.id).sort()).toEqual(["a", "c", "d"]);
    expect(out.edges).toEqual([
      { from: "a", to: "d" },
      { from: "a", to: "c" },
    ]);
  });

  it("job settings: the user's change wins, the rest is the saved version's", () => {
    const base = job([node("a")]);
    const mine = job([node("a")], { name: "Mine" });
    const theirs = job([node("a")], { schedule: { enabled: true, mode: "interval", minutes: 5, at: "08:00", weekdays: [] }, updated_at: 2 });
    const out = rebaseJob(base, mine, theirs);
    expect(out.name).toBe("Mine");
    expect(out.schedule.minutes).toBe(5);
  });
});
