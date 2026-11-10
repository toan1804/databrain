import { expect, test } from "@playwright/test";
import { backend, emit, openApp, SHOP } from "./app";

const N = 15;
const SQL = Array.from({ length: N }, (_, i) => (i === 11 ? "select * from orders_by_month" : `select ${i + 1} as n`));
const doc = SQL.map((s) => `${s};`).join("\n");
let at = 0;
const statements = SQL.map((sql, index) => {
  const start = at;
  at += sql.length + 2;
  return { index, sql, start, end: start + sql.length, classification: { kind: "read", missing_where: false, keyword: "SELECT" } };
});
const result = (i: number) => ({
  id: `res-${i}`,
  columns: [{ name: "n", data_type: "Int32", db_type: "int4", family: "number" }],
  total_rows: i + 1,
  complete: true,
  truncated: false,
  bytes: 10,
});

test("15 results in a tab: the strip scrolls, keeps the shown one in view, and the list jumps to any", async ({ page }) => {
  await page.setViewportSize({ width: 900, height: 700 });
  await openApp(
    page,
    backend({
      load_tabs: [{ id: "tab-1", title: "Query 1", sql: doc, connection_id: SHOP.id }],
      run_query: { status: "started", job_id: "job-1", statements },
      fetch_page: { view_rows: 1, total_rows: 1, offset: 0, row_ids: [0], rows: [["1"]] },
    }),
  );
  await page.getByRole("button", { name: "Run all" }).click();
  for (let i = 0; i < N; i++) {
    await emit(page, "job-event", {
      type: "statement_finished",
      job_id: "job-1",
      tab_id: "tab-1",
      index: i,
      result: result(i),
      output: null,
      rows_affected: null,
      duration_ms: 3,
      notices: [],
    });
  }
  await emit(page, "job-event", { type: "job_finished", job_id: "job-1", tab_id: "tab-1", status: "success", duration_ms: 50, in_transaction: false });

  const strip = page.getByRole("tablist", { name: "Results" });
  const tabs = strip.getByRole("tab");
  await expect(tabs).toHaveCount(N);
  // The last result is shown, and scrolled into view.
  await expect(tabs.nth(N - 1)).toHaveAttribute("aria-selected", "true");
  await expect(tabs.nth(N - 1)).toBeInViewport();
  await expect(page.getByRole("button", { name: "Messages" })).toBeInViewport();

  // The strip overflows: arrows scroll it back to the first result.
  const left = page.getByRole("button", { name: "Scroll results left" });
  await expect(left).toBeEnabled();
  await expect(tabs.first()).not.toBeInViewport();
  // Quick clicks add up (each one pages further than the last, even mid-scroll).
  const width = await strip.evaluate((el) => el.scrollLeft);
  const clicks = Math.ceil(width / (await strip.evaluate((el) => Math.max(120, el.clientWidth * 0.7))));
  for (let i = 0; i < clicks; i++) await left.click();
  await expect.poll(() => strip.evaluate((el) => el.scrollLeft)).toBe(0);
  await expect(left).toBeDisabled();
  await tabs.first().click();
  await expect(tabs.first()).toHaveAttribute("aria-selected", "true");

  // The keyboard moves through the results and keeps the selection visible.
  await page.keyboard.press("End");
  await expect(tabs.nth(N - 1)).toHaveAttribute("aria-selected", "true");
  await expect(tabs.nth(N - 1)).toBeInViewport();

  // The list of all results: find one by its SQL.
  await page.getByRole("button", { name: "All results" }).click();
  await page.getByRole("textbox", { name: "Find a result" }).fill("orders_by");
  await page.keyboard.press("Enter");
  await expect(tabs.nth(11)).toHaveAttribute("aria-selected", "true");
  await expect(tabs.nth(11)).toBeInViewport();
});
