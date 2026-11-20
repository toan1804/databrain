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
    // Columns first: no rows are read until Data is opened.
    await expect(tab.getByRole("tab", { name: /^Columns/ })).toHaveAttribute("aria-selected", "true");
    await expect(tab.getByRole("cell", { name: "customer_id" })).toBeVisible();
    expect(await calls(page, "run_query")).toHaveLength(0);
    // Data: the first 100 rows run when it opens, on the table tab's own run.
    await tab.getByRole("tab", { name: "Data" }).click();
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
  await tab.getByRole("tab", { name: "Data" }).click();
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
  await tab.getByRole("tab", { name: "Data" }).click();
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

test("Relations and Usage: keys both ways, learned joins, job steps and queries from the history", async ({ page }) => {
  await openApp(
    page,
    backend({
      run_query: {
        status: "started",
        job_id: "job-t",
        statements: [{ index: 0, sql: "SELECT …", start: 0, end: 8, classification: { kind: "read", missing_where: false, keyword: "SELECT" } }],
      },
      fetch_page: { view_rows: ROWS.length, total_rows: ROWS.length, offset: 0, row_ids: ROWS.map((_, i) => i), rows: ROWS },
      schema_tables: {
        offline: null,
        stale: false,
        tables: [
          { table: "orders", columns: [{ name: "id", data_type: "integer", nullable: false, is_primary_key: true }], foreign_keys: [] },
          { table: "order_items", columns: [{ name: "order_id", data_type: "integer", nullable: false, is_primary_key: false }], foreign_keys: [{ columns: ["order_id"], ref_schema: "public", ref_table: "orders", ref_columns: ["id"] }] },
        ],
      },
      kn_learned: {
        supported: true,
        enabled: true,
        state: null,
        tables: [{ table: "public.orders", queries: 4, days: 3, last_seen: Date.now() - 3_600_000 }],
        joins: [
          { shown: true, hidden_reason: null, id: 1, left_table: "public.orders", left_columns: ["region"], right_table: "public.regions", right_columns: ["code"], status: "confirmed", declared: false, queries: 3, days: 2, ai_queries: 0, kinds: {}, first_seen: 1, last_seen: 2, documented: null },
        ],
      },
      kn_table_notes: { target: "public.orders", notes: [] },
      table_usage: {
        learned: { table: "public.orders", queries: 4, days: 3, last_seen: Date.now() - 3_600_000 },
        steps: [{ job_id: "j1", job_name: "Nightly", step_id: "n1", step_name: "load_orders", reads: false, writes: true, sure: false }],
        history: [
          { id: 1, origin: "user", connection_id: "conn-shop", connection_name: "Shop", sql: "select * from orders", started_at: Date.now() - 60_000, duration_ms: 3, rows: 2, status: "success", error: null, output_handle: null, result_id: null },
          { id: 2, origin: "job", connection_id: "conn-shop", connection_name: "Shop", sql: "SELECT * FROM orders", started_at: Date.now() - 120_000, duration_ms: 3, rows: 2, status: "success", error: null, output_handle: null, result_id: null },
          { id: 3, origin: "user", connection_id: "conn-shop", connection_name: "Shop", sql: "select * from orders_archive", started_at: Date.now(), duration_ms: 3, rows: 2, status: "success", error: null, output_handle: null, result_id: null },
        ],
      },
    }),
  );
  const tree = page.getByRole("treeitem");
  await tree.filter({ hasText: "Shop" }).first().click();
  await tree.filter({ hasText: /^orders/ }).dblclick();
  const tab = page.getByLabel(/^Table .*orders/);
  await expect(tab).toBeVisible();

  await tab.getByRole("tab", { name: /^Relations/ }).click();
  await expect(tab.getByText("order_items")).toBeVisible();
  await expect(tab.getByText(/approved · 3 queries on 2 days/)).toBeVisible();
  await expect(tab.getByText("· given to the AI")).toBeVisible();
  await tab.getByRole("button", { name: /ER diagram around it/ }).click();
  await expect(page.getByRole("tab", { name: /ER · public/ })).toBeVisible();

  await page.getByRole("tab", { name: /orders/ }).first().click();
  await tab.getByRole("tab", { name: /^Usage/ }).click();
  await expect(tab.getByText(/4 different queries on 3 days/)).toBeVisible();
  await expect(tab.getByRole("button", { name: "Nightly" })).toBeVisible();
  await expect(tab.getByText("writes it")).toBeVisible();
  // Named without its schema: hedged.
  await expect(tab.getByText(/may be another schema's orders/)).toBeVisible();
  // The two spellings of one query are one line, run twice; orders_archive isn't this table.
  await expect(tab.getByText("×2")).toBeVisible();
  await expect(tab.getByText(/orders_archive/)).toHaveCount(0);
  expect(await calls(page, "table_usage")).toContainEqual({ connectionId: "conn-shop", schema: "public", name: "orders" });
});

test("Indexes: indexes with their use, partitioning, checks and triggers; History says why not on Postgres", async ({ page }) => {
  await openApp(
    page,
    backend({
      run_query: {
        status: "started",
        job_id: "job-t",
        statements: [{ index: 0, sql: "SELECT …", start: 0, end: 8, classification: { kind: "read", missing_where: false, keyword: "SELECT" } }],
      },
      fetch_page: { view_rows: ROWS.length, total_rows: ROWS.length, offset: 0, row_ids: ROWS.map((_, i) => i), rows: ROWS },
      kn_table_notes: { target: "public.orders", notes: [] },
      table_layout: {
        indexes: [
          { name: "orders_pkey", columns: ["id"], unique: true, primary: true },
          { name: "orders_status_idx", columns: ["status"], unique: false, primary: false, method: "btree" },
        ],
        partition_by: ["created_at"],
        partition_kind: "range",
        cluster_by: [],
        requires_partition_filter: false,
        notes: [],
      },
    }),
  );
  // The catalog reads, by what they ask for.
  await page.evaluate(() => {
    (window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown> } }).__DB_MOCK__.handlers.explore_query = (a: { sql: string }) =>
      a.sql.includes("pg_stat_all_indexes")
        ? { columns: ["name", "scans", "bytes"], rows: [["orders_pkey", "1200", "16384"], ["orders_status_idx", "0", "8192"]], duration_ms: 2 }
        : a.sql.includes("pg_trigger")
          ? { columns: ["what", "name", "definition"], rows: [["trigger", "audit_orders", "CREATE TRIGGER audit_orders AFTER UPDATE ON public.orders"]], duration_ms: 3 }
          : { columns: ["what", "name", "definition"], rows: [["check", "positive_total", "CHECK ((total > (0)::numeric))"]], duration_ms: 3 };
  });
  const tree = page.getByRole("treeitem");
  await tree.filter({ hasText: "Shop" }).first().click();
  await tree.filter({ hasText: /^orders/ }).dblclick();
  const tab = page.getByLabel(/^Table .*orders/);
  await expect(tab).toBeVisible();

  await tab.getByRole("tab", { name: /^Indexes/ }).click();
  await expect(tab.getByText("orders_status_idx")).toBeVisible();
  await expect(tab.getByText("primary key")).toBeVisible();
  await expect(tab.getByText("1,200")).toBeVisible();
  // A plain index never used: marked (the primary key's scans aren't its job).
  await expect(tab.getByTitle(/^Never used for a scan/)).toHaveCount(1);
  await expect(tab.getByText(/Partitioned by range on/)).toBeVisible();
  await expect(tab.getByText("positive_total")).toBeVisible();
  await expect(tab.getByText("audit_orders", { exact: true })).toBeVisible();
  // Catalog reads, as the explorer's (one read each, in the history).
  const reads = (await calls(page, "explore_query")) as { sql: string }[];
  expect(reads.some((r) => r.sql.includes("pg_constraint"))).toBe(true);
  expect(reads.some((r) => r.sql.includes("pg_trigger"))).toBe(true);

  await tab.getByRole("tab", { name: /^History/ }).click();
  await expect(tab.getByText(/doesn't keep earlier versions of a table's rows/)).toBeVisible();
});

test("Indexes on MySQL: MariaDB's form of the checks fails, MySQL's runs next, and the triggers are read apart", async ({ page }) => {
  const my = { ...SHOP, config: { ...SHOP.config, kind: "mysql" } };
  await openApp(
    page,
    backend({
      list_connections: [my],
      run_query: { status: "started", job_id: "job-m", statements: [] },
      kn_table_notes: { target: "public.orders", notes: [] },
      table_layout: { indexes: [], partition_by: [], cluster_by: [], requires_partition_filter: false, notes: [] },
    }),
  );
  await page.evaluate(() => {
    (window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown> } }).__DB_MOCK__.handlers.explore_query = (a: { sql: string }) =>
      a.sql.includes("cc.TABLE_NAME")
        ? { __error: { kind: "query", message: "Unknown column 'cc.TABLE_NAME' in 'on clause'" } }
        : a.sql.includes("TRIGGERS")
          ? { columns: ["what", "name", "definition"], rows: [], duration_ms: 1 }
          : { columns: ["what", "name", "definition"], rows: [["check", "qty_positive", "(`qty` > 0)"]], duration_ms: 1 };
  });
  const tree = page.getByRole("treeitem");
  await tree.filter({ hasText: "Shop" }).first().click();
  await tree.filter({ hasText: /^orders/ }).dblclick();
  const tab = page.getByLabel(/^Table .*orders/);
  await expect(tab).toBeVisible();
  await tab.getByRole("tab", { name: /^Indexes/ }).click();
  await expect(tab.getByText("qty_positive")).toBeVisible();
  await expect(tab.getByText(/couldn't be read/)).toHaveCount(0);
  const checks = ((await calls(page, "explore_query")) as { sql: string }[]).filter((r) => r.sql.includes("CHECK_CONSTRAINTS"));
  expect(checks.map((r) => r.sql.includes("cc.TABLE_NAME"))).toEqual([true, false]);
});
