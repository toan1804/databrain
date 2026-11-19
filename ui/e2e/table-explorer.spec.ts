import { expect, test } from "@playwright/test";
import { answer, backend, calls, emit, openApp, SHOP } from "./app";

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
        kn_table_notes: {
          target: "public.orders",
          notes: [{ id: "n0", connection_id: SHOP.id, target: "public.orders.amount", body: "incl. VAT", author: "user", status: "approved", created_at: 1, column: "amount" }],
        },
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
    const req = (await calls(page, "run_query"))[0].req as { tab_id: string; sql: string; row_limit: number; origin?: string };
    // As the explorer's: one read only, and so in the history.
    expect(req.origin).toBe("explorer");
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

    // A condition that would write is refused before anything runs (the server checks the SQL).
    await answer(page, "check_sample_sql", { __error: { kind: "invalid", message: "The condition must be one condition: it can't end the statement (`;`) and start another" } });
    await tab.getByLabel("Where condition").fill("1=1; delete from orders");
    await tab.getByRole("button", { name: "Run" }).click();
    await expect(tab.getByRole("alert")).toContainText("can't end the statement");
    expect(await calls(page, "run_query")).toHaveLength(1);
    expect((await calls(page, "check_sample_sql")).at(-1)).toMatchObject({ sql: expect.stringContaining("1=1; delete from orders") });

    // A filter runs again with the condition.
    await answer(page, "check_sample_sql", null);
    await tab.getByLabel("Where condition").fill("amount > 100");
    await tab.getByRole("button", { name: "Run" }).click();
    await expect.poll(async () => (await calls(page, "run_query")).length).toBe(2);
    expect(((await calls(page, "run_query"))[1].req as { sql: string }).sql).toMatch(/WHERE amount > 100\s+LIMIT 100/);

    // Columns: types, keys (PK, FK) from the description.
    await tab.getByRole("tab", { name: /Columns/ }).click();
    await expect(tab.getByRole("cell", { name: "customer_id" })).toBeVisible();
    await expect(tab.getByText("PK")).toBeVisible();
    // The note on public.orders.amount is on amount, and only there.
    await expect(tab.getByRole("row", { name: /amount/ })).toContainText("incl. VAT");
    await expect(tab.getByRole("row", { name: /customer_id/ })).not.toContainText("incl. VAT");
    if (process.env.EXPLORER_SHOT) await page.screenshot({ path: process.env.EXPLORER_SHOT.replace(".png", "-columns.png") });

    // DDL: as Show DDL loads it.
    await tab.getByRole("tab", { name: "DDL" }).click();
    await expect(tab.getByText(/CREATE TABLE public\.orders/)).toBeVisible();

    // Notes: the column note; a new one is saved as the user's, with the target the server gave.
    await tab.getByRole("tab", { name: /Notes/ }).click();
    await expect(tab.getByRole("listitem").filter({ hasText: "incl. VAT" })).toBeVisible();
    await tab.getByLabel("Note", { exact: true }).fill("amount includes VAT");
    await tab.getByRole("button", { name: /Save note/ }).click();
    await expect.poll(async () => (await calls(page, "kn_save_note")).length).toBe(1);
    expect((await calls(page, "kn_save_note"))[0].note).toMatchObject({ target: "public.orders", body: "amount includes VAT", author: "user", status: "approved" });

    // Back to Data: the condition and its rows are as left (not reset).
    await tab.getByRole("tab", { name: "Data" }).click();
    await expect(tab.getByLabel("Where condition")).toHaveValue("amount > 100");

    // Opening it again shows the same tab.
    await tree.filter({ hasText: /^orders/ }).dblclick();
    await expect(page.getByLabel(/^Table .*orders/)).toHaveCount(1);
  });
});

test("on BigQuery the sample doesn't run on opening, and Run says what it bills first", async ({ page }) => {
  const bq = { ...SHOP, config: { ...SHOP.config, kind: "bigquery" } };
  await openApp(
    page,
    backend({
      list_connections: [bq],
      list_catalogs: [{ name: "proj", is_default: true }],
      list_catalog_schemas: [{ name: "proj.sales", is_default: true }],
      list_schemas: [{ name: "proj.sales", is_default: true }],
      list_objects: [{ schema: "proj.sales", name: "orders", kind: "table", comment: null, row_estimate: 50_000 }],
      run_query: { status: "started", job_id: "job-b", statements: [{ index: 0, sql: "SELECT …", start: 0, end: 8, classification: { kind: "read", missing_where: false, keyword: "SELECT" } }] },
      estimate_cost: { method: "dry_run", bytes: 5 * 1024 ** 3, cache_hit: false, notes: [] },
      kn_table_notes: { target: "public.orders", notes: [] },
    }),
  );
  const tree = page.getByRole("treeitem");
  await tree.filter({ hasText: "Shop" }).first().click();
  // BigQuery: project, then dataset.
  if (!(await tree.filter({ hasText: /^orders/ }).isVisible())) {
    await tree.filter({ hasText: /^proj/ }).first().click();
    if (!(await tree.filter({ hasText: /^orders/ }).isVisible())) await tree.filter({ hasText: /^sales/ }).first().click();
  }
  await tree.filter({ hasText: /^orders/ }).dblclick();
  const tab = page.getByLabel(/^Table .*orders/);
  await expect(tab.getByText(/Press Run to read sample rows/)).toBeVisible();
  expect(await calls(page, "run_query")).toHaveLength(0);
  await tab.getByRole("button", { name: "Run" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("about 5.0 GB");
  await dialog.getByRole("button", { name: "Read rows" }).click();
  await expect.poll(async () => (await calls(page, "run_query")).length).toBe(1);
});

test("on a production connection the sample asks once per tab before it reads", async ({ page }) => {
  const prod = { ...SHOP, env: "prod" };
  await openApp(
    page,
    backend({
      list_connections: [prod],
      run_query: { status: "started", job_id: "job-p", statements: [{ index: 0, sql: "SELECT …", start: 0, end: 8, classification: { kind: "read", missing_where: false, keyword: "SELECT" } }] },
      kn_table_notes: { target: "public.orders", notes: [] },
    }),
  );
  const tree = page.getByRole("treeitem");
  await tree.filter({ hasText: "Shop" }).first().click();
  await tree.filter({ hasText: /^orders/ }).dblclick();
  const tab = page.getByLabel(/^Table .*orders/);
  await expect(tab.getByText(/production connection, so they aren't read on opening/)).toBeVisible();
  expect(await calls(page, "run_query")).toHaveLength(0);
  await tab.getByRole("button", { name: "Run" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("production connection");
  await dialog.getByRole("button", { name: "Read rows" }).click();
  await expect.poll(async () => (await calls(page, "run_query")).length).toBe(1);
  await emit(page, "job-event", { type: "job_finished", job_id: "job-p", tab_id: ((await calls(page, "run_query"))[0].req as { tab_id: string }).tab_id, status: "success", duration_ms: 5, in_transaction: false });
  // Asked once: the next Run reads at once.
  await tab.getByRole("button", { name: "Run" }).click();
  await expect.poll(async () => (await calls(page, "run_query")).length).toBe(2);
  await expect(page.getByRole("dialog")).toHaveCount(0);
});
