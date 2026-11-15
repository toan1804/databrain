import { expect, test, type Page } from "@playwright/test";
import { answer, backend, calls, emit, openApp } from "./app";

type SavedJob = { id: string; nodes: { id: string; name: string; sql: string }[]; edges: { from: string; to: string }[] };

/** The job as the app last saved it. */
async function lastSaved(page: Page): Promise<SavedJob | undefined> {
  const saves = await calls(page, "save_job");
  return saves.at(-1)?.job as SavedJob | undefined;
}

async function newJob(page: Page) {
  await page.getByRole("button", { name: "Jobs", exact: true }).click();
  await page.getByRole("button", { name: "New job" }).first().click();
  await expect(page.getByRole("tab", { name: /Job 1/ })).toBeVisible();
  return page.getByLabel("Job steps");
}

test.describe("job canvas", () => {
  test("a new job has one step; steps are added and linked by dragging", async ({ page }) => {
    await openApp(page);
    const canvas = await newJob(page);
    const cards = canvas.locator("[data-node]");
    await expect(cards).toHaveCount(1);
    await expect(cards.first()).toContainText("Shop");

    await page.getByRole("button", { name: "Step", exact: true }).click();
    await expect(cards).toHaveCount(2);
    await expect.poll(async () => (await lastSaved(page))?.nodes.length).toBe(2);

    // Drag the first step's handle onto the second: the second runs after it.
    const [first, second] = [cards.nth(0), cards.nth(1)];
    const handle = first.getByRole("button", { name: /to a downstream step/ });
    const from = (await handle.boundingBox())!;
    const to = (await second.boundingBox())!;
    await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
    await page.mouse.down();
    await page.mouse.move(to.x + to.width / 2, to.y + to.height / 2, { steps: 8 });
    await page.mouse.up();
    await expect(canvas.locator("[data-edge]")).toHaveCount(1);
    const job = (await lastSaved(page))!;
    await expect.poll(async () => (await lastSaved(page))?.edges.length).toBe(1);
    const saved = (await lastSaved(page))!;
    expect(saved.edges[0]).toEqual({ from: job.nodes[0].id, to: job.nodes[1].id });
  });

  test("selecting a step shows its details; renaming it is saved", async ({ page }) => {
    await openApp(page);
    const canvas = await newJob(page);
    await canvas.locator("[data-node]").first().click();
    const details = page.getByLabel("Step details");
    const name = details.getByRole("textbox").first();
    await expect(name).toHaveValue("step");
    await name.fill("daily_orders");
    await expect(canvas.locator("[data-node]").first()).toContainText("daily_orders");
    await expect.poll(async () => (await lastSaved(page))?.nodes[0].name).toBe("daily_orders");
  });

  test("running a job shows each step's progress from backend events", async ({ page }) => {
    const step = { id: "n1", name: "orders_today", kind: "query", connection_id: "conn-shop", sql: "select * from orders", x: 80, y: 80, load_mode: "insert" };
    const job = { id: "job-orders", name: "Orders", nodes: [step], edges: [], schedule: { enabled: false, mode: "interval", minutes: 60, at: "08:00", weekdays: [] }, created_at: 1, updated_at: 1 };
    await openApp(page, backend({ __jobs: [job] }));
    await page.getByRole("button", { name: "Jobs", exact: true }).click();
    await page.getByText("Orders", { exact: true }).click();
    const card = page.getByLabel("Job steps").locator("[data-node]").first();
    await expect(card).toContainText("Not run yet");

    const started = Date.now();
    const run = (id: number, status: string, s: Record<string, unknown>) => ({
      id,
      job_id: job.id,
      trigger: "manual",
      started_at: started,
      finished_at: status === "running" ? null : started + 1200,
      status,
      nodes: [{ node_id: "n1", name: "orders_today", finished_at: 0, duration_ms: 0, ...s }],
      error: null,
    });
    await answer(page, "run_job", run(7, "running", { status: "running" }));
    await page.getByRole("button", { name: "Run job" }).click();
    await expect.poll(async () => (await calls(page, "run_job")).length).toBe(1);
    expect((await calls(page, "run_job"))[0]).toMatchObject({ id: job.id });

    await emit(page, "job-run", { job_id: job.id, run: run(7, "running", { status: "running" }) });
    await expect(card).toContainText("Running");
    await emit(page, "job-run", { job_id: job.id, run: run(7, "success", { status: "success", rows: 42, duration_ms: 1200, finished_at: started + 1200 }) });
    await expect(card).toContainText("42 rows");
    await emit(page, "job-run", { job_id: job.id, run: run(8, "error", { status: "error", error: 'relation "orders" does not exist' }) });
    await expect(card).toContainText('relation "orders" does not exist');
  });
});

test.describe("the assistant changes a job with unsaved edits here", () => {
  const step = { id: "n1", name: "orders_today", kind: "query", connection_id: "conn-shop", sql: "select * from orders", x: 80, y: 80, load_mode: "insert" };
  const job = { id: "job-orders", name: "Orders", nodes: [step], edges: [], schedule: { enabled: false, mode: "interval", minutes: 60, at: "08:00", weekdays: [] }, created_at: 1, updated_at: 1 };

  /** The job open, its step renamed here but not saved (the save fails), then the assistant's update_job. */
  async function conflict(page: Page) {
    await openApp(page, backend({ __jobs: [job] }));
    await page.getByRole("button", { name: "Jobs", exact: true }).click();
    await page.getByText("Orders", { exact: true }).click();
    const card = page.getByLabel("Job steps").locator("[data-node]").first();
    await expect(card).toContainText("orders_today");
    await answer(page, "save_job", { __error: { kind: "internal", message: "disk full" } });
    await card.click();
    await page.getByLabel("Step details").getByRole("textbox").first().fill("mine");
    await expect.poll(async () => (await calls(page, "save_job")).length).toBe(1);
    // The assistant saved its version meanwhile; saving works again.
    await page.evaluate((j) => {
      const w = window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown>; db: { jobs: unknown[] } } };
      delete w.__DB_MOCK__.handlers.save_job;
      w.__DB_MOCK__.db.jobs = [{ ...j, nodes: [{ ...j.nodes[0], name: "assistants" }], updated_at: 2 }];
      window.dispatchEvent(new CustomEvent("db:job-changed", { detail: { id: j.id } }));
    }, job);
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("The assistant changed this job");
    return { card, dialog };
  }

  test("closing the question shows the assistant's version and saves nothing over it", async ({ page }) => {
    const { card, dialog } = await conflict(page);
    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    await expect(card).toContainText("assistants");
    await expect(page.getByLabel("Step details").getByRole("textbox").first()).toHaveValue("assistants");
    // Longer than the name check (250 ms) and the save delay (600 ms) together.
    await page.waitForTimeout(1500);
    expect(await calls(page, "save_job")).toHaveLength(1);
  });

  test("Cancel shows the assistant's version too", async ({ page }) => {
    const { card, dialog } = await conflict(page);
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(card).toContainText("assistants");
    await expect(page.getByLabel("Step details").getByRole("textbox").first()).toHaveValue("assistants");
    await page.waitForTimeout(1500);
    expect(await calls(page, "save_job")).toHaveLength(1);
  });

  test("Keep my changes saves them over the assistant's", async ({ page }) => {
    const { card, dialog } = await conflict(page);
    await dialog.getByRole("button", { name: "Keep my changes" }).click();
    await expect.poll(async () => (await lastSaved(page))?.nodes[0].name).toBe("mine");
    await expect(card).toContainText("mine");
    await page.waitForTimeout(1500);
    expect(await calls(page, "save_job")).toHaveLength(2);
  });
});
