import { expect, test } from "@playwright/test";
import { backend, calls, openApp } from "./app";

test("Settings → Editor: inline completion can use another provider than the AI panel's (Kiro can't)", async ({ page }) => {
  const cfg = (default_model: string | null, fast_model: string | null = null) => ({ base_url: null, auth: "api_key", default_model, fast_model, api_version: null, extra_headers: {}, max_output_tokens: null });
  const kiro = { has_key: false, id: "prov-kiro", kind: "kiro", name: "Kiro", config: { ...cfg("auto"), auth: "kiro_browser" }, created_at: 1, updated_at: 1 };
  const ollama = { has_key: false, id: "prov-ollama", kind: "ollama", name: "Local Ollama", config: cfg("llama3.1", "qwen2.5-coder:1.5b"), created_at: 2, updated_at: 2 };
  await openApp(page, backend({ ai_list_providers: [kiro, ollama], ai_list_models: [{ id: "qwen2.5-coder:1.5b" }, { id: "qwen2.5-coder:7b" }] }));
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("button", { name: "Editor", exact: true }).click();
  const provider = dialog.getByLabel("Provider", { exact: true });
  // The AI panel's provider is Kiro: it says so, and Kiro isn't offered.
  await expect(provider).toHaveValue("");
  await expect(dialog.getByText(/The AI panel uses Kiro/)).toBeVisible();
  await expect(provider.locator("option")).toHaveText(["Same as the AI panel (Kiro)", "Local Ollama"]);

  await provider.selectOption("prov-ollama");
  await expect(dialog.getByText(/The AI panel uses Kiro/)).toHaveCount(0);
  const model = dialog.getByLabel("Model", { exact: true });
  await expect(model).toHaveAttribute("placeholder", "qwen2.5-coder:1.5b (the provider's fast model)");
  await model.fill("qwen2.5-coder:7b");
  await expect
    .poll(async () => (await calls(page, "set_setting")).filter((c) => c.key === "inline_ai_provider").at(-1)?.value)
    .toEqual({ provider_id: "prov-ollama", model: "qwen2.5-coder:7b" });
  await page.screenshot({ path: "test-results/settings-inline-provider.png" });
  // Back to the panel's.
  await provider.selectOption("");
  await expect.poll(async () => (await calls(page, "set_setting")).filter((c) => c.key === "inline_ai_provider").at(-1)?.value).toBeNull();
});
