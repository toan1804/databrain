import { expect, test } from "@playwright/test";
import { backend, openApp } from "./app";

test("Settings → MCP is for any agent that supports MCP, not one product", async ({ page }) => {
  await openApp(page, backend({ mcp_config: { mcpServers: { databrain: { command: "/Applications/DataBrain.app/Contents/MacOS/databrain-mcp", args: [] } } } }));
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("button", { name: "MCP / Kiro" })).toHaveCount(0);
  await dialog.getByRole("button", { name: "MCP", exact: true }).click();
  await expect(dialog.getByText(/any AI agent that supports the Model Context Protocol/)).toBeVisible();
  await expect(dialog.locator("pre")).toContainText('"mcpServers"');
  // Agents with another config format get the command to run.
  await expect(dialog.getByText("/Applications/DataBrain.app/Contents/MacOS/databrain-mcp", { exact: true })).toBeVisible();
  await page.screenshot({ path: "test-results/settings-mcp.png" });
});
