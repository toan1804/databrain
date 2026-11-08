import { expect, test } from "@playwright/test";
import { backend, calls, openApp } from "./app";

const TABLES = ["customers", "orders", "order_items", "products", "payments", "shipments", "invoices", "returns"];
const join = (id: number, l: string, r: string, lc: string, rc: string, over: Record<string, unknown> = {}) => ({
  id,
  left_table: `sales.${l}`,
  left_columns: [lc],
  right_table: `sales.${r}`,
  right_columns: [rc],
  status: "learned",
  declared: false,
  queries: 40 - id,
  days: 3,
  ai_queries: 0,
  kinds: { inner: 2 },
  first_seen: Date.now() - 86_400_000 * 5,
  last_seen: Date.now() - 3_600_000 as number | null,
  shown: true,
  hidden_reason: null as string | null,
  ...over,
});
// Many joins: given, waiting (used once) and rejected ones.
const JOINS = TABLES.flatMap((l, i) =>
  TABLES.slice(i + 1).map((r, k) => {
    const id = i * 10 + k + 1;
    const waiting = id % 3 === 0;
    return join(id, l, r, `${r.replace(/s$/, "")}_id`, "id", waiting ? { queries: 1, days: 1, shown: false, hidden_reason: "Used in 1 query on 1 day (needs 2)" } : {});
  }),
);
JOINS[4] = { ...JOINS[4], status: "rejected", shown: false, hidden_reason: "Rejected" };

test("Knowledge: index, notes and learned joins each have a tab; joins are one line each", async ({ page }) => {
  await openApp(
    page,
    backend({
      kn_get: {
        state: { connection_id: "conn-shop", indexed_at: Date.now() - 60_000, objects: 120, schemas: ["sales"], error: null },
        notes: [
          { id: "n1", connection_id: "conn-shop", target: "sales.orders", body: "Cancelled orders have status 9", author: "user", status: "approved", created_at: 0 },
          { id: "n2", connection_id: "conn-shop", target: null, body: "GMV = sum(amount) of paid orders", author: "ai", status: "proposed", created_at: 0 },
        ],
      },
      kn_objects: { objects: [], total: 0 },
      kn_learned: {
        supported: true,
        enabled: true,
        state: { last_history_id: 10, learned_at: Date.now() - 120_000, error: null },
        joins: JOINS,
        tables: TABLES.map((t, i) => ({ table: `sales.${t}`, queries: 100 - i * 9, days: 10 - i, last_seen: Date.now() })),
      },
      kn_join_evidence: [{ id: 7, origin: "user", connection_id: "conn-shop", connection_name: "Shop", sql: "SELECT * FROM sales.customers c JOIN sales.orders o ON o.customer_id = c.id", started_at: Date.now() - 3_600_000, duration_ms: 5, rows: 3, status: "ok", error: null, output_handle: null, result_id: null }],
    }),
  );
  await page.getByRole("button", { name: "AI assistant" }).first().click();
  await page.getByRole("tab", { name: /Knowledge/ }).click();

  const tabs = page.getByRole("tablist", { name: "Knowledge" });
  await expect(tabs.getByRole("tab", { name: /Index\s*120/ })).toBeVisible();
  // One approved note, one proposed by the AI to review.
  await expect(tabs.getByRole("tab", { name: /Notes\s*1\s*1/ })).toBeVisible();
  await tabs.getByRole("tab", { name: new RegExp(`Learned\\s*${JOINS.length}`) }).click();

  const list = page.getByRole("list", { name: "Learned joins" });
  await expect(list.getByRole("listitem")).toHaveCount(JOINS.length);
  await page.screenshot({ path: "test-results/knowledge-learned.png" });

  // Filter by status and by table.
  const waiting = JOINS.filter((j) => !j.shown && j.status !== "rejected").length;
  await page.getByRole("button", { name: new RegExp(`Waiting\\s*${waiting}`) }).click();
  await expect(list.getByRole("listitem")).toHaveCount(waiting);
  await page.getByRole("button", { name: /^All/ }).click();
  await page.getByLabel("Filter learned").fill("customers orders");
  await expect(list.getByRole("listitem")).toHaveCount(1);

  // Nothing to confirm (the AI gets a join from use); a wrong one is rejected from the line.
  const item = list.getByRole("listitem").first();
  await item.hover();
  await expect(item.getByRole("button", { name: /Confirm/ })).toHaveCount(0);
  await item.getByRole("button", { name: "Reject" }).click();
  await expect.poll(async () => (await calls(page, "kn_set_join_status")).at(-1)).toMatchObject({ id: JOINS[0].id, status: "rejected" });
  await item.getByRole("button", { name: /customers/ }).first().click();
  await expect(item.getByText("Queries it came from")).toBeVisible();
  await expect(item.getByText(/JOIN sales\.orders o ON o\.customer_id = c\.id/)).toBeVisible();
  // One schema for all: said once, full names when opened.
  await expect(item.getByText("sales.customers.order_id = sales.orders.id")).toBeVisible();
  await expect(item.getByRole("button", { name: "Make note" })).toBeVisible();
  await expect(item.getByRole("button", { name: /Confirm/ })).toHaveCount(0);
  // A rejected join can be undone.
  const rejected = list.getByRole("listitem").filter({ hasText: "shipment_id" });
  await page.getByLabel("Filter learned").fill("shipment_id customers");
  await rejected.hover();
  await rejected.getByRole("button", { name: "Undo reject" }).click();
  await expect.poll(async () => (await calls(page, "kn_set_join_status")).at(-1)).toMatchObject({ id: JOINS[4].id, status: "learned" });
  await page.screenshot({ path: "test-results/knowledge-learned-open.png" });

  // Most used tables.
  await page.getByLabel("Filter learned").fill("");
  await page.getByRole("tab", { name: /Tables\s*8/ }).click();
  await expect(page.getByRole("list", { name: "Most used tables" }).getByRole("listitem")).toHaveCount(TABLES.length);
});

test("Knowledge remembers the last tab", async ({ page }) => {
  await openApp(page, backend({ kn_get: { state: null, notes: [] }, kn_objects: { objects: [], total: 0 }, kn_learned: { supported: true, enabled: false, state: null, joins: [], tables: [] } }));
  await page.getByRole("button", { name: "AI assistant" }).first().click();
  await page.getByRole("tab", { name: /Knowledge/ }).click();
  await page.getByRole("tablist", { name: "Knowledge" }).getByRole("tab", { name: /Learned/ }).click();
  await expect(page.getByText(/Off for this connection/)).toBeVisible();
  await page.reload();
  // The panel may come back open on Knowledge by itself.
  const kn = page.getByRole("tablist", { name: "Knowledge" });
  if (!(await kn.isVisible().catch(() => false))) {
    if (!(await page.getByRole("tab", { name: /Knowledge/ }).isVisible())) await page.getByRole("button", { name: "AI assistant" }).first().click();
    await page.getByRole("tab", { name: /Knowledge/ }).click();
  }
  await expect(page.getByRole("tablist", { name: "Knowledge" }).getByRole("tab", { name: /Learned/ })).toHaveAttribute("aria-selected", "true");
});
