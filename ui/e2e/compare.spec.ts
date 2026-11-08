import { expect, test } from "@playwright/test";
import { answer, backend, calls, openApp, SHOP } from "./app";

const meta = (name: string, data_type: string, family: string) => ({ name, data_type, db_type: null, family });
const output = (handle: string, name: string, connection_name: string, rows: number, columns: ReturnType<typeof meta>[]) => ({
  handle,
  name,
  version_of: null,
  result_id: `res-${handle}`,
  connection_id: "conn-shop",
  connection_name,
  kind: "postgres",
  sql: `select * from ${name}`,
  // Not a tab's current result (no grid shows it).
  tab_id: "tab-gone",
  statement_index: 0,
  created_at: Date.now(),
  rows,
  columns,
  truncated: false,
  row_limit: null,
  bytes: 1000,
  pinned: false,
  state: "live",
  origin: "user",
  last_used: Date.now(),
  active: false,
});

const BEFORE = output("r1", "orders_ora", "Oracle prod", 3, [meta("id", "Int32", "number"), meta("d", "Timestamp(µs)", "date"), meta("amt", "Decimal128(10, 3)", "number"), meta("code", "Utf8", "text")]);
const AFTER = output("r2", "orders_pg", "Postgres DWH", 4, [meta("id", "Utf8", "text"), meta("d", "Utf8", "text"), meta("amt", "Utf8", "text"), meta("code", "Utf8", "text")]);
const pair = (c: string, bt: string, kind: string) => ({ before: c, after: c, before_type: bt, after_type: "Utf8", auto_kind: kind, kind });
const PAIRS = [pair("id", "Int32", "number"), pair("d", "Timestamp(µs)", "date_time"), pair("amt", "Decimal128(10, 3)", "number"), pair("code", "Utf8", "text")];
const RESULT_COLS = ["_change", "id", "d", "amt", "code", "d (before)", "amt (before)", "code (before)", "_changed"];
const ROWS = [
  ["added", "4", "n/a", "abc", "new", null, null, null, null],
  ["changed", "3", "2026-11-13", "5.01", "zed", "2026-11-13 00:00:00", "5.004", "Zed", "amt, code"],
];

test("compare two outputs: counts, changed cells and filters", async ({ page }) => {
  await openApp(page, backend({ list_outputs: [AFTER, BEFORE] }));
  await answer(page, "output_compare_pairs", PAIRS);
  await answer(page, "compare_outputs", {
    summary: {
      before_rows: 3,
      after_rows: 4,
      same: 2,
      changed: 1,
      added: 1,
      removed: 0,
      before_duplicate_keys: 0,
      after_duplicate_keys: 0,
      columns: [
        { column: "d", changed: 0 },
        { column: "amt", changed: 1 },
        { column: "code", changed: 1 },
      ],
    },
    result: { id: "diff-1", columns: RESULT_COLS.map((n) => meta(n, "Utf8", "text")), total_rows: 2, complete: true, truncated: false, bytes: 100 },
    sql: "-- Differences from r1 (before) to r2 (after)\nSELECT 1;",
    pairs: PAIRS,
    keys: ["id"],
    before: BEFORE,
    after: AFTER,
  });
  // Pages honour the filters on `_change` (column 0) and `_changed` (column 8).
  await page.evaluate(
    ([rows]) => {
      const w = window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown> } };
      w.__DB_MOCK__.handlers.fetch_page = (a: { view: { filters: { column: number; value: string }[] } }) => {
        const keep = (rows as (string | null)[][]).filter((r) => a.view.filters.every((f) => (f.column === 0 ? r[0] === f.value : (r[f.column] ?? "").includes(f.value))));
        return { view_rows: keep.length, total_rows: 2, offset: 0, row_ids: keep.map((_, i) => i), rows: keep };
      };
    },
    [ROWS] as const,
  );

  await page.getByRole("button", { name: "Outputs", exact: true }).click();
  await page.getByRole("button", { name: "Compare outputs" }).click();
  const dialog = page.getByRole("dialog");
  // Detected kinds and the two types, per column.
  await expect(dialog.getByLabel("Compare d as")).toHaveValue("");
  await expect(dialog.getByLabel("Compare d as").locator("option").first()).toHaveText("Auto · date/time");
  await expect(dialog.getByText("timestamp → text")).toBeVisible();
  await dialog.getByText("id", { exact: true }).first().click(); // key
  await dialog.getByRole("button", { name: "Show differences" }).click();

  // A compare tab with the counts, and the rows.
  await expect(page.getByRole("tab", { name: /Compare orders_ora → orders_pg/ })).toBeVisible();
  await expect(page.getByRole("button", { name: /1\s*Changed/ })).toBeVisible();
  await expect(page.getByRole("button", { name: /1\s*Only after/ })).toBeVisible();
  await expect(page.getByText(/2\s*Same/)).toBeVisible();
  const spec = (await calls(page, "compare_outputs")).at(-1)?.spec as { keys: string[]; options: { trim: boolean } };
  expect(spec.keys).toEqual(["id"]);
  expect(spec.options.trim).toBe(true);

  const table = page.locator("table").last();
  await expect(table.getByText("only after")).toBeVisible();
  // Changed cells show before → after; d didn't change, so "only columns that differ" hides it.
  const changed = table.locator("tr", { hasText: "changed" });
  await expect(changed).toContainText("5.004→5.01");
  await expect(changed).toContainText("Zed→zed");
  await expect(table.locator("th", { hasText: /^d$/ })).toHaveCount(0);
  await page.getByLabel("Only columns that differ").uncheck();
  await expect(table.locator("th", { hasText: /^d$/ })).toHaveCount(1);
  await page.screenshot({ path: "test-results/compare-view.png" });
  // Nothing went wrong on the way (an error shows as a toast).
  await expect(page.getByText(/Cannot read|failed/i)).toHaveCount(0);

  // A count filters the rows; a column chip filters changed rows by column.
  await page.getByRole("button", { name: /1\s*Only after/ }).click();
  await expect(table.locator("tbody tr")).toHaveCount(1);
  await expect(table).toContainText("only after");
  await page.getByRole("button", { name: /amt\s*1/ }).click();
  await expect(table.locator("tbody tr")).toHaveCount(1);
  await expect(table).toContainText("changed");
});

test("compare a table with the same table on another connection, from the explorer", async ({ page }) => {
  const DWH = { ...SHOP, id: "conn-dwh", name: "Shop DWH" };
  const cols = [meta("id", "Int32", "number"), meta("name", "Utf8", "text"), meta("email", "Utf8", "text")];
  await openApp(page, backend({ list_connections: [SHOP, DWH] }));
  // Reading a side makes an output (listed from then on).
  await page.evaluate((c) => {
    const w = window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown> } };
    const outs: unknown[] = [];
    let n = 0;
    w.__DB_MOCK__.handlers.list_outputs = () => [...outs];
    w.__DB_MOCK__.handlers.compare_load_source = (a: { source: { connection_id: string; table: string } }) => {
      n += 1;
      const o = {
        handle: `r${n}`,
        name: null,
        version_of: null,
        result_id: `res-r${n}`,
        connection_id: a.source.connection_id,
        connection_name: a.source.connection_id === "conn-dwh" ? "Shop DWH" : "Shop",
        kind: "postgres",
        sql: `select * from public.${a.source.table}`,
        tab_id: "compare:dialog",
        statement_index: 0,
        created_at: Date.now(),
        rows: 1200,
        columns: c,
        truncated: false,
        row_limit: 1000000,
        bytes: 1000,
        pinned: false,
        state: "live",
        origin: "user",
        last_used: Date.now(),
        active: false,
      };
      outs.unshift(o);
      return o;
    };
  }, cols);
  await answer(page, "output_compare_pairs", ["id", "name", "email"].map((c) => ({ before: c, after: c, before_type: "Int32", after_type: "Int32", auto_kind: "text", kind: "text" })));
  await answer(page, "compare_outputs", {
    summary: { before_rows: 1200, after_rows: 1200, same: 1200, changed: 0, added: 0, removed: 0, before_duplicate_keys: 0, after_duplicate_keys: 0, columns: [] },
    result: { id: "diff-2", columns: ["_change", "id", "name", "email"].map((n) => meta(n, "Utf8", "text")), total_rows: 0, complete: true, truncated: false, bytes: 0 },
    sql: "SELECT 1;",
    pairs: [],
    keys: ["id"],
    before: { handle: "r1", name: null, connection_name: "Shop", sql: "", rows: 1200, truncated: false },
    after: { handle: "r2", name: null, connection_name: "Shop DWH", sql: "", rows: 1200, truncated: false },
  });
  await answer(page, "fetch_page", { view_rows: 0, total_rows: 0, offset: 0, row_ids: [], rows: [] });

  await page.getByRole("treeitem").filter({ hasText: "Shop" }).first().click();
  await page.getByRole("treeitem").filter({ hasText: /^customers/ }).click({ button: "right" });
  await page.getByText("Compare data with…").click();
  const dialog = page.getByRole("dialog");
  // Before: read right away. After: the same table, on a connection to pick.
  await expect.poll(async () => (await calls(page, "compare_load_source")).length).toBe(1);
  expect((await calls(page, "compare_load_source"))[0]).toMatchObject({ source: { kind: "table", connection_id: "conn-shop", schema: "public", table: "customers" }, side: "before" });
  await expect(dialog.getByLabel("After table")).toHaveValue("customers");
  await dialog.getByLabel("After connection").selectOption("conn-dwh");
  await expect(dialog.getByLabel("After schema")).toHaveValue("public");
  await page.screenshot({ path: "test-results/compare-dialog.png" });
  await dialog.getByRole("button", { name: "Read" }).click();
  await expect.poll(async () => (await calls(page, "compare_load_source")).length).toBe(2);
  // The table's primary key is suggested as the key.
  await expect(dialog.getByText(/Suggested: the table's primary key/)).toBeVisible();
  await dialog.getByRole("button", { name: "Show differences" }).click();

  await expect.poll(async () => (await calls(page, "compare_outputs")).length).toBeGreaterThan(0);
  expect((await calls(page, "compare_outputs")).at(-1)).toMatchObject({ spec: { before: "r1", after: "r2", keys: ["id"] } });
  await expect(page.getByText("Shop · public.customers")).toBeVisible();
  await expect(page.getByText("Shop DWH · public.customers")).toBeVisible();
  await expect(page.getByText("No differences: the outputs hold the same data.")).toBeVisible();
  // Compare again reads both tables again, for this tab.
  await page.getByRole("button", { name: "Compare again" }).click();
  await expect.poll(async () => (await calls(page, "compare_load_source")).length).toBe(4);
  const again = (await calls(page, "compare_load_source")).slice(2);
  expect(again.map((c) => c.side)).toEqual(["before", "after"]);
  expect(again.every((c) => c.tabId !== "dialog")).toBe(true);
  await expect(page.getByText(/Cannot read|failed/i)).toHaveCount(0);
});
