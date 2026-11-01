import { expect, test } from "@playwright/test";
import { answer, backend, calls, openApp } from "./app";

test.describe("explorer", () => {
  test("lists schemas, tables and columns of a connection", async ({ page }) => {
    await openApp(page);
    const tree = page.getByRole("treeitem");
    await tree.filter({ hasText: "Shop" }).first().click();
    // The default schema opens by itself and lists its objects by kind.
    await expect(tree.filter({ hasText: "public" })).toBeVisible();
    await expect(tree.filter({ hasText: /^orders/ })).toBeVisible();
    await expect(tree.filter({ hasText: /^customers/ })).toBeVisible();
    await expect(tree.filter({ hasText: /revenue_by_day/ })).toBeVisible();
    expect(await calls(page, "list_objects")).toContainEqual({ id: "conn-shop", schema: "public" });

    // A table opens to its columns (described on demand).
    await tree.filter({ hasText: /^orders/ }).click();
    await expect(tree.filter({ hasText: "customer_id" })).toBeVisible();
    await expect(tree.filter({ hasText: "amount" })).toBeVisible();
    expect(await calls(page, "describe_object")).toContainEqual({ id: "conn-shop", schema: "public", name: "orders" });
  });

  test("a schema's context menu opens its ER diagram", async ({ page }) => {
    await openApp(
      page,
      backend({
        schema_tables: [
          { table: "customers", columns: [{ name: "id", data_type: "integer", nullable: false, is_primary_key: true }], foreign_keys: [] },
          {
            table: "orders",
            columns: [
              { name: "id", data_type: "integer", nullable: false, is_primary_key: true },
              { name: "customer_id", data_type: "integer", nullable: true, is_primary_key: false },
            ],
            foreign_keys: [{ columns: ["customer_id"], ref_schema: "public", ref_table: "customers", ref_columns: ["id"] }],
          },
        ],
      }),
    );
    await page.getByRole("treeitem").filter({ hasText: "Shop" }).first().click();
    await page.getByRole("treeitem").filter({ hasText: "public" }).click({ button: "right" });
    await page.getByRole("button", { name: "Show ER diagram" }).click();
    await expect(page.getByRole("tab", { name: /ER · public/ })).toBeVisible();
    await expect(page.getByText("2 tables · 1 foreign key")).toBeVisible();
    const node = (id: string) => page.locator(`svg[aria-label="ER diagram"] [data-node="${id}"]`).first();
    await expect(node("public.orders")).toContainText("customer_id");
    await expect(node("public.customers")).toContainText("PK integer");
    expect(await calls(page, "schema_tables")).toContainEqual({ id: "conn-shop", schema: "public" });

    // Filtering hides tables; Refresh reads the schema again.
    await page.getByRole("textbox", { name: "Filter tables" }).fill("cust");
    await expect(node("public.orders")).toHaveCount(0);
    await expect(node("public.customers")).toBeVisible();
    await answer(page, "schema_tables", []);
    await page.getByRole("button", { name: "Refresh" }).click();
    await expect(page.getByText("This schema has no tables.")).toBeVisible();
  });

  test("an unreachable schema shows the error", async ({ page }) => {
    await openApp(page, backend({ list_objects: { __error: { kind: "connection", message: "Connection refused by db.local:5432" } } }));
    await page.getByRole("treeitem").filter({ hasText: "Shop" }).first().click();
    await expect(page.getByText("Connection refused by db.local:5432").first()).toBeVisible();
  });
});
