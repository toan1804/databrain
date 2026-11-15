import { expect, test } from "@playwright/test";
import { backend, calls, openApp } from "./app";

const meta = (name: string) => ({ name, data_type: "Utf8", db_type: "text", family: "text" });
// A result of another tab, on another connection.
const WAREHOUSE = {
  handle: "r7",
  name: "orders_dwh",
  version_of: null,
  result_id: "res-r7",
  connection_id: "conn-dwh",
  connection_name: "Warehouse",
  kind: "postgres",
  sql: "select id, status from orders",
  tab_id: "tab-other",
  statement_index: 0,
  created_at: Date.now(),
  rows: 2,
  columns: [meta("id"), meta("status")],
  truncated: false,
  row_limit: null,
  bytes: 100,
  pinned: false,
  spilled: false,
  state: "live",
  origin: "user",
  last_used: Date.now(),
  active: true,
  kept: false,
};

test("+ adds another tab's result here, read-only, and × closes it", async ({ page }) => {
  await openApp(
    page,
    backend({
      list_outputs: [WAREHOUSE],
      fetch_page: { view_rows: 2, total_rows: 2, offset: 0, row_ids: [0, 1], rows: [["1", "paid"], ["2", "open"]] },
    }),
  );
  await page.evaluate((o) => {
    const w = window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown> } };
    w.__DB_MOCK__.handlers.pin_output = (a: { handle: string; pinned: boolean }) => ({ ...o, handle: a.handle, pinned: a.pinned });
  }, WAREHOUSE);

  // Nothing run in this tab yet: add from the empty results.
  await page.getByRole("button", { name: "Add a result from another tab" }).click();
  const picker = page.getByRole("dialog", { name: "Add a result" });
  await picker.getByLabel("Find a result to add").fill("orders");
  await picker.getByRole("option", { name: /orders_dwh/ }).click();

  // A tab of its own, read-only: no Edit, no Keep; pinned so it stays.
  const strip = page.getByRole("tablist", { name: "Results" });
  await expect(strip.getByRole("tab", { name: /Warehouse/ })).toHaveAttribute("aria-selected", "true");
  await expect(page.getByText("Read-only", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Edit", exact: true })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Keep result" })).toHaveCount(0);
  await expect(page.getByText("2 rows", { exact: true })).toBeVisible();
  await expect.poll(async () => (await calls(page, "pin_output")).at(-1)).toEqual({ handle: "r7", pinned: true });
  await page.screenshot({ path: "test-results/add-result.png" });

  // Not offered again while it is here.
  await page.getByRole("button", { name: "Add a result", exact: true }).click();
  await expect(page.getByRole("dialog", { name: "Add a result" })).toContainText("No other results yet");
  await page.keyboard.press("Escape");

  // Closed: unpinned (adding pinned it), and the tab is empty again.
  await page.getByRole("button", { name: "Close orders_dwh" }).click();
  await expect(strip).toHaveCount(0);
  await expect.poll(async () => (await calls(page, "pin_output")).at(-1)).toEqual({ handle: "r7", pinned: false });
});
