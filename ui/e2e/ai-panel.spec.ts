import { expect, test, type Page } from "@playwright/test";
import { answer, calls, emit, openApp } from "./app";

const RUN = "run-1";
const SESSION = "sess-1";

async function ask(page: Page, question: string) {
  await answer(page, "ai_send", RUN);
  await page.getByRole("button", { name: "AI assistant" }).first().click();
  const box = page.getByRole("textbox", { name: "Message the assistant" });
  await box.fill(question);
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect.poll(async () => (await calls(page, "ai_send")).length).toBe(1);
  await emit(page, "ai-event", { type: "started", session_id: SESSION, run_id: RUN });
}

test.describe("AI panel", () => {
  test("sends the question with the connection and streams the answer", async ({ page }) => {
    await openApp(page);
    await ask(page, "Which customers ordered most last month?");
    const sent = (await calls(page, "ai_send"))[0].args as Record<string, unknown>;
    expect(sent).toMatchObject({ connection_id: "conn-shop", provider_id: "prov-1", message: "Which customers ordered most last month?" });

    await expect(page.getByText("Which customers ordered most last month?").first()).toBeVisible();
    await emit(page, "ai-event", { type: "text_delta", session_id: SESSION, run_id: RUN, text: "The top customer is " });
    await emit(page, "ai-event", { type: "text_delta", session_id: SESSION, run_id: RUN, text: "**Binh**." });
    await expect(page.getByText(/The top customer is/).first()).toBeVisible();
    await emit(page, "ai-event", { type: "finished", session_id: SESSION, run_id: RUN, text: "The top customer is **Binh**." });
    await expect(page.locator("strong", { hasText: "Binh" }).first()).toBeVisible();
  });

  test("a query the assistant wants to run waits for approval", async ({ page }) => {
    await openApp(page);
    await ask(page, "Count the orders");
    await emit(page, "ai-event", {
      type: "approval_request",
      request_id: "req-1",
      run_id: RUN,
      tool: "run_query",
      summary: "Count orders",
      detail: { sql: "select count(*) from orders", connection: "Shop" },
    });
    await expect(page.getByText("select count(*) from orders").first()).toBeVisible();
    await page.getByRole("button", { name: "Run", exact: true }).last().click();
    await expect.poll(async () => (await calls(page, "ai_respond")).map((c) => c.requestId)).toContain("req-1");
    const reply = (await calls(page, "ai_respond")).find((c) => c.requestId === "req-1")!;
    expect(reply.response).toMatchObject({ approved: true });
  });

  test("a failed turn shows the provider's error", async ({ page }) => {
    await openApp(page);
    await ask(page, "Hello");
    await emit(page, "ai-event", { type: "failed", session_id: SESSION, run_id: RUN, error: { kind: "provider", message: "rate limit reached for gpt-test" } });
    await expect(page.getByText(/rate limit reached for gpt-test/).first()).toBeVisible();
  });
});
