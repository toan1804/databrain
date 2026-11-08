import { expect, test, type Page } from "@playwright/test";
import { backend, openApp } from "./app";

/** Every button of the toolbar row ends inside the row (nothing cut off on the right). */
async function fits(page: Page, names: string[]) {
  for (const name of names) {
    const button = page.getByRole("button", { name, exact: true }).last();
    await expect(button).toBeVisible();
    const box = await button.boundingBox();
    const view = page.viewportSize();
    expect(box && view && box.x + box.width <= view.width, `${name} is cut off`).toBeTruthy();
  }
}

async function openAiPanel(page: Page) {
  await page.getByRole("button", { name: "AI assistant" }).first().click();
  await expect(page.getByRole("textbox", { name: "Message the assistant" })).toBeVisible();
}

test.describe("toolbars fit when the AI panel is open", () => {

  test("DDL tab: buttons drop their labels", async ({ page }) => {
    await openApp(page, backend({ object_ddl: "CREATE TABLE public.customers (id integer PRIMARY KEY, name text, email text);" }));
    const tree = page.getByRole("treeitem");
    await tree.filter({ hasText: "Shop" }).first().click();
    await tree.filter({ hasText: /^customers/ }).click({ button: "right" });
    await page.getByRole("button", { name: /Show DDL/ }).click();
    await expect(page.getByText("CREATE TABLE").first()).toBeVisible();
    await expect(page.getByRole("button", { name: "Open in editor" })).toContainText("Open in editor");

    await openAiPanel(page);
    await expect(page.getByRole("button", { name: "Open in editor" })).not.toContainText("Open in editor");
    await fits(page, ["Format", "Copy", "Open in editor", "Reload"]);
  });

  test("ER tab: buttons drop their labels", async ({ page }) => {
    await openApp(
      page,
      backend({ schema_tables: [{ table: "customers", columns: [{ name: "id", data_type: "integer", nullable: false, is_primary_key: true }], foreign_keys: [] }] }),
    );
    await page.getByRole("treeitem").filter({ hasText: "Shop" }).first().click();
    await page.getByRole("treeitem").filter({ hasText: "public" }).click({ button: "right" });
    await page.getByRole("button", { name: "Show ER diagram" }).click();
    await expect(page.getByRole("button", { name: "Fit" })).toContainText("Fit");

    await openAiPanel(page);
    await expect(page.getByRole("button", { name: "Fit" })).not.toContainText("Fit");
    await fits(page, ["Fit", "Refresh", "Export"]);
  });
});
