// README screenshots: the UI in Chromium against the mocked backend, with
// sample data. `npm run screenshots` writes them to docs/screenshots/.
import { expect, test, type Page } from "@playwright/test";
import { answer, backend, emit, openApp } from "../app";

const OUT = "../docs/screenshots";

const SQL = `-- Revenue per customer this month
select c.name, count(*) as orders, sum(o.amount) as revenue, max(o.created_at) as last_order
from public.orders o
join public.customers c on c.id = o.customer_id
where o.created_at >= date_trunc('month', now())
group by c.name
order by revenue desc;`;

const COLUMNS = [
  { name: "name", data_type: "Utf8", db_type: "text", family: "text" },
  { name: "orders", data_type: "Int64", db_type: "int8", family: "number" },
  { name: "revenue", data_type: "Utf8", db_type: "numeric", family: "number" },
  { name: "last_order", data_type: "Timestamp(Microsecond, Some(\"UTC\"))", db_type: "timestamptz", family: "date" },
];
const NAMES = ["Binh Tran", "Ana Souza", "Kenji Sato", "Lena Fischer", "Omar Haddad", "Priya Nair", "Lucas Martin", "Mei Chen", "Jonas Berg", "Sara Rossi", "Diego Ruiz", "Hana Kim", "Tom Walsh", "Aisha Bello", "Nina Petrova", "Ravi Shah", "Eva Novak", "Leo Dubois", "Maya Cohen", "Ken Ito"];
const ROWS = NAMES.map((n, i) => [n, String(48 - i * 2), (9840.5 - i * 412.25).toFixed(2), `2026-11-${String(28 - (i % 20)).padStart(2, "0")} ${String(9 + (i % 9)).padStart(2, "0")}:${String((i * 7) % 60).padStart(2, "0")}:00+00`]);
const RESULT = { id: "res-1", columns: COLUMNS, total_rows: ROWS.length, complete: true, truncated: false, bytes: 4096 };

async function shot(page: Page, name: string) {
  // Let fonts, the canvas grid and transitions settle.
  await page.waitForTimeout(400);
  await page.screenshot({ path: `${OUT}/${name}.png` });
}

/** Open the explorer on the Shop connection with `orders` expanded. */
async function explorer(page: Page) {
  const tree = page.getByRole("treeitem");
  await tree.filter({ hasText: "Shop" }).first().click();
  await tree.filter({ hasText: /^orders/ }).click();
  await expect(tree.filter({ hasText: "customer_id" })).toBeVisible();
}

test.describe("screenshots", () => {
  test.use({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, colorScheme: "dark" });

  test("query and results", async ({ page }) => {
    await openApp(page, backend({ load_tabs: [{ id: "tab-1", title: "Revenue", sql: SQL, connection_id: "conn-shop" }] }));
    await explorer(page);
    await answer(page, "run_query", { status: "started", job_id: "job-1", statements: [{ index: 0, sql: SQL, start: 0, end: SQL.length, classification: { kind: "read", missing_where: false, keyword: "SELECT" } }] });
    await answer(page, "fetch_page", { view_rows: ROWS.length, total_rows: ROWS.length, offset: 0, row_ids: ROWS.map((_, i) => i), rows: ROWS });
    await page.locator(".cm-content").first().click();
    await page.keyboard.press("ControlOrMeta+Shift+Enter");
    await expect.poll(async () => page.evaluate(() => (window as unknown as { __DB_MOCK__: { callsOf: (c: string) => unknown[] } }).__DB_MOCK__.callsOf("run_query").length)).toBe(1);
    const ev = { job_id: "job-1", tab_id: "tab-1", index: 0 };
    await emit(page, "job-event", { type: "statement_started", ...ev, sql: SQL });
    await emit(page, "job-event", { type: "statement_finished", ...ev, result: RESULT, output: null, rows_affected: null, duration_ms: 182, notices: [] });
    await emit(page, "job-event", { type: "job_finished", job_id: "job-1", tab_id: "tab-1", status: "success", duration_ms: 190, in_transaction: false });
    await expect(page.getByText(/20 rows/).first()).toBeVisible();
    await shot(page, "query-results");
  });

  test("AI assistant", async ({ page }) => {
    await openApp(page, backend({ load_tabs: [{ id: "tab-1", title: "Revenue", sql: SQL, connection_id: "conn-shop" }] }));
    await explorer(page);
    await answer(page, "ai_send", "run-1");
    await page.getByRole("button", { name: "AI assistant" }).first().click();
    await page.getByRole("textbox", { name: "Message the assistant" }).fill("Which customers ordered most this month?");
    await page.getByRole("button", { name: "Send", exact: true }).click();
    const base = { session_id: "sess-1", run_id: "run-1" };
    await emit(page, "ai-event", { type: "started", ...base });
    const text =
      "Binh Tran ordered most this month: **48 orders**, 9,840.50 in revenue. The top five:\n\n" +
      "| name | orders | revenue |\n|---|---|---|\n| Binh Tran | 48 | 9840.50 |\n| Ana Souza | 46 | 9428.25 |\n| Kenji Sato | 44 | 9016.00 |\n| Lena Fischer | 42 | 8603.75 |\n| Omar Haddad | 40 | 8191.50 |\n\n" +
      "The query I used:\n\n```sql\nselect c.name, count(*) as orders, sum(o.amount) as revenue\nfrom public.orders o\njoin public.customers c on c.id = o.customer_id\nwhere o.created_at >= date_trunc('month', now())\ngroup by c.name\norder by revenue desc\nlimit 5;\n```";
    await emit(page, "ai-event", { type: "text_delta", ...base, text });
    await emit(page, "ai-event", { type: "finished", ...base, text });
    await expect(page.locator("strong", { hasText: "48 orders" }).first()).toBeVisible();
    await shot(page, "ai-assistant");
  });

  test("ER diagram", async ({ page }) => {
    const c = (name: string, data_type: string, pk = false) => ({ name, data_type, nullable: !pk, is_primary_key: pk });
    await openApp(
      page,
      backend({
        schema_tables: { offline: null, stale: false, tables: [
          { table: "customers", columns: [c("id", "integer", true), c("name", "text"), c("email", "text"), c("country", "text")], foreign_keys: [] },
          { table: "products", columns: [c("id", "integer", true), c("sku", "text"), c("title", "text"), c("price", "numeric(12,2)")], foreign_keys: [] },
          {
            table: "orders",
            columns: [c("id", "integer", true), c("customer_id", "integer"), c("amount", "numeric(12,2)"), c("created_at", "timestamptz")],
            foreign_keys: [{ columns: ["customer_id"], ref_schema: "public", ref_table: "customers", ref_columns: ["id"] }],
          },
          {
            table: "order_items",
            columns: [c("order_id", "integer", true), c("product_id", "integer", true), c("quantity", "integer"), c("unit_price", "numeric(12,2)")],
            foreign_keys: [
              { columns: ["order_id"], ref_schema: "public", ref_table: "orders", ref_columns: ["id"] },
              { columns: ["product_id"], ref_schema: "public", ref_table: "products", ref_columns: ["id"] },
            ],
          },
        ] },
      }),
    );
    await page.getByRole("treeitem").filter({ hasText: "Shop" }).first().click();
    await page.getByRole("treeitem").filter({ hasText: "public" }).click({ button: "right" });
    await page.getByRole("button", { name: "Show ER diagram" }).click();
    await expect(page.getByText("4 tables · 3 foreign keys")).toBeVisible();
    await shot(page, "er-diagram");
  });

  test("job canvas", async ({ page }) => {
    const step = (id: string, name: string, x: number, y: number, extra: Record<string, unknown> = {}) => ({ id, name, kind: "query", connection_id: "conn-shop", sql: `select * from ${name}`, x, y, load_mode: "insert", ...extra });
    const job = {
      id: "job-daily",
      name: "Daily revenue",
      nodes: [
        step("n1", "orders_today", 80, 80, { sql: "select * from public.orders where created_at >= current_date" }),
        step("n2", "customers", 380, 80, { sql: "select id, name, country from public.customers" }),
        step("n3", "revenue_by_country", 230, 260, { connection_id: null, sql: "select c.country, sum(o.amount) revenue\nfrom results.orders_today o join results.customers c on c.id = o.customer_id\ngroup by 1" }),
      ],
      edges: [
        { from: "n1", to: "n3" },
        { from: "n2", to: "n3" },
      ],
      schedule: { enabled: true, mode: "daily", minutes: 60, at: "07:00", weekdays: [] },
      created_at: 1,
      updated_at: 1,
    };
    await openApp(page, backend({ __jobs: [job] }));
    await page.getByRole("button", { name: /^Jobs/ }).click();
    await page.getByText("Daily revenue", { exact: true }).click();
    await expect(page.getByLabel("Job steps").locator("[data-node]")).toHaveCount(3);
    await shot(page, "job-canvas");
  });
});
