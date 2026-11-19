import { expect, test } from "@playwright/test";
import { backend, calls, emit, openApp, SHOP } from "./app";

const ROWS = [
  ["1", "77", "120.50"],
  ["2", "12", "35.00"],
];

test.describe("table explorer", () => {
  test("double-clicking a table opens its tab: sample rows, columns, DDL and notes", async ({ page }) => {
    await openApp(
      page,
      backend({
        run_query: {
          status: "started",
          job_id: "job-t",
          statements: [{ index: 0, sql: "SELECT …", start: 0, end: 8, classification: { kind: "read", missing_where: false, keyword: "SELECT" } }],
        },
        fetch_page: { view_rows: ROWS.length, total_rows: ROWS.length, offset: 0, row_ids: ROWS.map((_, i) => i), rows: ROWS },
        object_ddl: "CREATE TABLE public.orders (id integer PRIMARY KEY, customer_id integer REFERENCES public.customers(id));",
        kn_table_notes: [],
        kn_save_note: { id: "n1", connection_id: SHOP.id, target: "public.orders", body: "x", author: "user", status: "approved", created_at: 1 },
      }),
    );
    const tree = page.getByRole("treeitem");
    await tree.filter({ hasText: "Shop" }).first().click();
    await tree.filter({ hasText: /^orders/ }).dblclick();

    const tab = page.getByLabel(/^Table .*orders/);
    await expect(tab).toBeVisible();
    await expect(page.getByRole("tab", { name: /orders/ }).first()).toBeVisible();
    // Data: the first 100 rows run at once, on the table tab's own run.
    await expect.poll(async () => (await calls(page, "run_query")).length).toBe(1);
    const req = (await calls(page, "run_query"))[0].req as { tab_id: string; sql: string; row_limit: number };
    expect(req.sql).toMatch(/FROM .*orders/i);
    expect(req.sql).toMatch(/LIMIT 100/i);
    expect(req.row_limit).toBe(100);
    await emit(page, "job-event", {
      type: "statement_finished",
      job_id: "job-t",
      tab_id: req.tab_id,
      index: 0,
      result: {
        id: "res-t",
        columns: ["id", "customer_id", "amount"].map((name) => ({ name, data_type: "Utf8", db_type: "text", family: "text" })),
        total_rows: ROWS.length,
        complete: true,
        truncated: false,
        bytes: 100,
      },
      output: null,
      rows_affected: null,
      duration_ms: 3,
      notices: [],
    });
    await emit(page, "job-event", { type: "job_finished", job_id: "job-t", tab_id: req.tab_id, status: "success", duration_ms: 5, in_transaction: false });
    await expect(tab.getByTestId("data-grid-canvas")).toBeVisible();
    if (process.env.EXPLORER_SHOT) await page.screenshot({ path: process.env.EXPLORER_SHOT });

    // A filter runs again with the condition.
    await tab.getByLabel("Where condition").fill("amount > 100");
    await tab.getByRole("button", { name: "Run" }).click();
    await expect.poll(async () => (await calls(page, "run_query")).length).toBe(2);
    expect(((await calls(page, "run_query"))[1].req as { sql: string }).sql).toMatch(/WHERE amount > 100\s+LIMIT 100/);

    // Columns: types, keys (PK, FK) from the description.
    await tab.getByRole("tab", { name: /Columns/ }).click();
    await expect(tab.getByRole("cell", { name: "customer_id" })).toBeVisible();
    await expect(tab.getByText("PK")).toBeVisible();
    if (process.env.EXPLORER_SHOT) await page.screenshot({ path: process.env.EXPLORER_SHOT.replace(".png", "-columns.png") });

    // DDL: as Show DDL loads it.
    await tab.getByRole("tab", { name: "DDL" }).click();
    await expect(tab.getByText(/CREATE TABLE public\.orders/)).toBeVisible();

    // Notes: none yet; a new one is saved as the user's, about the table.
    await tab.getByRole("tab", { name: /Notes/ }).click();
    await expect(tab.getByText(/No notes about this table yet/)).toBeVisible();
    await tab.getByLabel("Note", { exact: true }).fill("amount includes VAT");
    await tab.getByRole("button", { name: /Save note/ }).click();
    await expect.poll(async () => (await calls(page, "kn_save_note")).length).toBe(1);
    expect((await calls(page, "kn_save_note"))[0].note).toMatchObject({ target: expect.stringMatching(/orders/), body: "amount includes VAT", author: "user", status: "approved" });

    // Opening it again shows the same tab.
    await tree.filter({ hasText: /^orders/ }).dblclick();
    await expect(page.getByLabel(/^Table .*orders/)).toHaveCount(1);
  });
});
