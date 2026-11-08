import { expect, test } from "@playwright/test";
import { answer, backend, calls, openApp } from "./app";

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
