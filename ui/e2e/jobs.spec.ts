import { expect, test, type Page } from "@playwright/test";
import { answer, backend, calls, emit, openApp } from "./app";

type SavedJob = { id: string; updated_at: number; nodes: { id: string; name: string; sql: string; ai_sql?: boolean }[]; edges: { from: string; to: string }[] };

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
    await expect(dialog).toContainText("This job was changed elsewhere");
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

  test("Keep my changes saves them on top of the assistant's version", async ({ page }) => {
    const { card, dialog } = await conflict(page);
    await dialog.getByRole("button", { name: "Keep my changes" }).click();
    await expect.poll(async () => (await lastSaved(page))?.nodes[0].name).toBe("mine");
    await expect(card).toContainText("mine");
    await page.waitForTimeout(1500);
    expect(await calls(page, "save_job")).toHaveLength(2);
    // Based on the assistant's version: the app takes it.
    expect((await lastSaved(page))?.updated_at).toBe(2);
  });

  test("Keep my changes never brings back the assistant's older SQL as the user's", async ({ page }) => {
    const aiStep = { ...step, sql: "select * from orders where day = current_date", ai_sql: true };
    await openApp(page, backend({ __jobs: [{ ...job, nodes: [aiStep] }] }));
    await page.getByRole("button", { name: "Jobs", exact: true }).click();
    await page.getByText("Orders", { exact: true }).click();
    const card = page.getByLabel("Job steps").locator("[data-node]").first();
    await expect(card).toContainText("orders_today");
    // A save that fails, then the user renames the step (not its SQL).
    await answer(page, "save_job", { __error: { kind: "internal", message: "disk full" } });
    await card.click();
    await page.getByLabel("Step details").getByRole("textbox").first().fill("mine");
    await expect.poll(async () => (await calls(page, "save_job")).length).toBe(1);
    // The assistant changes the step's SQL meanwhile.
    await page.evaluate((j) => {
      const w = window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown>; db: { jobs: unknown[] } } };
      delete w.__DB_MOCK__.handlers.save_job;
      w.__DB_MOCK__.db.jobs = [{ ...j, updated_at: 2 }];
      window.dispatchEvent(new CustomEvent("db:job-changed", { detail: { id: j.id } }));
    }, { ...job, nodes: [{ ...aiStep, sql: "select * from orders where day >= current_date - 7" }] });
    await page.getByRole("dialog").getByRole("button", { name: "Keep my changes" }).click();
    await expect.poll(async () => (await lastSaved(page))?.nodes[0].name).toBe("mine");
    const saved = await lastSaved(page);
    expect(saved?.nodes[0].sql).toBe("select * from orders where day >= current_date - 7");
    expect(saved?.nodes[0].ai_sql).toBe(true);
  });

  // A save sent and not answered when the assistant saves its own change:
  // the change event and the refused save both arrive, in either order. One
  // dialog asks, and "Keep my changes" keeps the edits that save carried.
  for (const order of ["event first", "refusal first"] as const) {
    test(`edits in a save under way are kept when the assistant saves meanwhile (${order})`, async ({ page }) => {
      await openApp(page, backend({ __jobs: [job] }));
      await page.getByRole("button", { name: "Jobs", exact: true }).click();
      await page.getByText("Orders", { exact: true }).click();
      const card = page.getByLabel("Job steps").locator("[data-node]").first();
      await expect(card).toContainText("orders_today");
      // Hold the editor's save until the assistant has saved.
      await page.evaluate(() => {
        const w = window as unknown as {
          __DB_MOCK__: { handlers: Record<string, unknown>; builtins: Record<string, (a: unknown) => unknown> };
          __release?: () => void;
        };
        w.__DB_MOCK__.handlers.save_job = (a: unknown) =>
          new Promise((res) => {
            w.__release = () => {
              delete w.__DB_MOCK__.handlers.save_job;
              res(w.__DB_MOCK__.builtins.save_job(a));
            };
          });
      });
      await card.click();
      await page.getByLabel("Step details").getByRole("textbox").first().fill("mine");
      await expect.poll(async () => (await calls(page, "save_job")).length).toBe(1);
      // The assistant saves a change of its own (a newer version).
      await page.evaluate((j) => {
        const w = window as unknown as { __DB_MOCK__: { db: { jobs: unknown[] } } };
        w.__DB_MOCK__.db.jobs = [{ ...j, nodes: [{ ...j.nodes[0], sql: "select 2" }], updated_at: 50 }];
      }, job);
      const event = () =>
        page.evaluate((id) => window.dispatchEvent(new CustomEvent("db:job-changed", { detail: { id } })), job.id);
      const refusal = () => page.evaluate(() => (window as unknown as { __release: () => void }).__release());
      if (order === "event first") {
        await event();
        await refusal();
      } else {
        await refusal();
        await event();
      }
      const dialog = page.getByRole("dialog");
      await expect(dialog).toContainText("This job was changed elsewhere");
      await dialog.getByRole("button", { name: "Keep my changes" }).click();
      await expect.poll(async () => (await lastSaved(page))?.nodes[0].name).toBe("mine");
      const saved = await lastSaved(page);
      expect(saved?.nodes[0].sql).toBe("select 2");
      expect(saved?.updated_at).toBe(50);
      await expect(dialog).toHaveCount(0);
      await expect(card).toContainText("mine");
    });
  }

  // Edits keep coming while a save is slow: the saves waiting behind it go
  // out one after another, each on the version the one before saved, so
  // none is refused as "changed elsewhere" when nothing else changed.
  test("saves waiting behind a slow one aren't refused as changed elsewhere", async ({ page }) => {
    await openApp(page, backend({ __jobs: [job] }));
    await page.getByRole("button", { name: "Jobs", exact: true }).click();
    await page.getByText("Orders", { exact: true }).click();
    const card = page.getByLabel("Job steps").locator("[data-node]").first();
    await expect(card).toContainText("orders_today");
    // Hold the first save.
    await page.evaluate(() => {
      const w = window as unknown as {
        __DB_MOCK__: { handlers: Record<string, unknown>; builtins: Record<string, (a: unknown) => unknown> };
        __release?: () => void;
      };
      w.__DB_MOCK__.handlers.save_job = (a: unknown) =>
        new Promise((res) => {
          w.__release = () => {
            delete w.__DB_MOCK__.handlers.save_job;
            res(w.__DB_MOCK__.builtins.save_job(a));
          };
        });
    });
    await card.click();
    const name = page.getByLabel("Step details").getByRole("textbox").first();
    await name.fill("first");
    await expect.poll(async () => (await calls(page, "save_job")).length).toBe(1);
    // Two more edits, each saved after the pause (two saves now wait).
    await name.fill("second");
    await page.waitForTimeout(900);
    await name.fill("third");
    await page.waitForTimeout(900);
    await page.evaluate(() => (window as unknown as { __release: () => void }).__release());
    await expect.poll(async () => (await lastSaved(page))?.nodes[0].name).toBe("third");
    await expect(page.getByRole("dialog")).toHaveCount(0);
    const stored = await page.evaluate(() => (window as unknown as { __DB_MOCK__: { db: { jobs: { nodes: { name: string }[] }[] } } }).__DB_MOCK__.db.jobs[0].nodes[0].name);
    expect(stored).toBe("third");
    await expect(page.getByText(/changed elsewhere/)).toHaveCount(0);
  });

  // Cancel gives up the edits of a save waiting behind a slow one too: it
  // isn't sent once the slow one answers, even before the assistant's
  // version has loaded, so no second dialog asks about them.
  test("Cancel drops a save waiting behind a slow one", async ({ page }) => {
    await openApp(page, backend({ __jobs: [job] }));
    await page.getByRole("button", { name: "Jobs", exact: true }).click();
    await page.getByText("Orders", { exact: true }).click();
    const card = page.getByLabel("Job steps").locator("[data-node]").first();
    await expect(card).toContainText("orders_today");
    // Hold the first save.
    await page.evaluate(() => {
      const w = window as unknown as {
        __DB_MOCK__: { handlers: Record<string, unknown>; builtins: Record<string, (a: unknown) => unknown> };
        __release?: () => void;
      };
      w.__DB_MOCK__.handlers.save_job = (a: unknown) =>
        new Promise((res) => {
          w.__release = () => {
            delete w.__DB_MOCK__.handlers.save_job;
            res(w.__DB_MOCK__.builtins.save_job(a));
          };
        });
    });
    await card.click();
    const name = page.getByLabel("Step details").getByRole("textbox").first();
    await name.fill("first");
    await expect.poll(async () => (await calls(page, "save_job")).length).toBe(1);
    // A second edit: its save waits behind the first.
    await name.fill("second");
    await page.waitForTimeout(900);
    // The assistant saves; loading its version is slow.
    await page.evaluate((j) => {
      const w = window as unknown as {
        __DB_MOCK__: { handlers: Record<string, unknown>; builtins: Record<string, (a: unknown) => unknown>; db: { jobs: unknown[] } };
        __loaded?: () => void;
      };
      w.__DB_MOCK__.db.jobs = [{ ...j, nodes: [{ ...j.nodes[0], name: "assistants" }], updated_at: 50 }];
      w.__DB_MOCK__.handlers.get_job = (a: unknown) =>
        new Promise((res) => {
          w.__loaded = () => {
            delete w.__DB_MOCK__.handlers.get_job;
            res(w.__DB_MOCK__.builtins.get_job(a));
          };
        });
      window.dispatchEvent(new CustomEvent("db:job-changed", { detail: { id: j.id } }));
    }, job);
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("This job was changed elsewhere");
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toHaveCount(0);
    // The first save is refused (an older version); the one behind it is dropped.
    await page.evaluate(() => (window as unknown as { __release: () => void }).__release());
    await page.waitForTimeout(500);
    await page.evaluate(() => (window as unknown as { __loaded: () => void }).__loaded());
    await expect(card).toContainText("assistants");
    await page.waitForTimeout(1500);
    await expect(dialog).toHaveCount(0);
    expect(await calls(page, "save_job")).toHaveLength(1);
    const stored = await page.evaluate(() => (window as unknown as { __DB_MOCK__: { db: { jobs: { nodes: { name: string }[] }[] } } }).__DB_MOCK__.db.jobs[0].nodes[0].name);
    expect(stored).toBe("assistants");
  });

  test("a save refused as made on an older version asks too", async ({ page }) => {
    await openApp(page, backend({ __jobs: [job] }));
    await page.getByRole("button", { name: "Jobs", exact: true }).click();
    await page.getByText("Orders", { exact: true }).click();
    const card = page.getByLabel("Job steps").locator("[data-node]").first();
    await expect(card).toContainText("orders_today");
    // Saved elsewhere without the event reaching this tab.
    await page.evaluate((j) => {
      const w = window as unknown as { __DB_MOCK__: { db: { jobs: unknown[] } } };
      w.__DB_MOCK__.db.jobs = [{ ...j, nodes: [{ ...j.nodes[0], sql: "select 2" }], updated_at: 5 }];
    }, job);
    await card.click();
    await page.getByLabel("Step details").getByRole("textbox").first().fill("mine");
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("This job was changed elsewhere");
    await dialog.getByRole("button", { name: "Keep my changes" }).click();
    await expect.poll(async () => (await lastSaved(page))?.nodes[0].name).toBe("mine");
    expect((await lastSaved(page))?.nodes[0].sql).toBe("select 2");
  });
});

test.describe("the activity bar", () => {
  test("a job running while the user is in an editor takes a row below it, not a card over it", async ({ page }) => {
    const step = { id: "n1", name: "load_orders", kind: "query", connection_id: "conn-shop", sql: "select * from orders", x: 80, y: 80, load_mode: "insert" };
    const job = { id: "job-orders", name: "Nightly sales", nodes: [step], edges: [], schedule: { enabled: false, mode: "interval", minutes: 60, at: "08:00", weekdays: [] }, created_at: 1, updated_at: 1 };
    await openApp(page, backend({ __jobs: [job] }));
    const started = Date.now() - 65_000;
    const progress = { phase: "Inserting rows", done: 250_000, total: 1_000_000, elapsed_ms: 10_000 };
    const run = (status: string, node: Record<string, unknown>) => ({ id: 7, job_id: job.id, trigger: "schedule", started_at: started, finished_at: status === "running" ? null : started + 90_000, status, nodes: [{ node_id: "n1", name: "load_orders", finished_at: 0, duration_ms: 0, ...node }], error: null });
    const bar = page.getByRole("region", { name: "Background activity" });
    await expect(bar).toHaveCount(0);

    await emit(page, "job-run", { job_id: job.id, run: run("running", { status: "running", progress }) });
    await expect(bar).toContainText("Nightly sales · load_orders: Inserting rows: 250,000 / 1,000,000 rows · 25%");
    // Slid in (its height animates).
    await expect.poll(() => bar.evaluate((el) => el.parentElement!.getBoundingClientRect().height)).toBe(24);
    // Its own row: the editor ends where the bar starts.
    const barBox = (await bar.boundingBox())!;
    const editor = (await page.locator(".cm-editor").first().boundingBox())!;
    expect(editor.y + editor.height).toBeLessThanOrEqual(barBox.y + 1);
    expect(barBox.y + barBox.height).toBeLessThanOrEqual(page.viewportSize()!.height + 1);
    await page.screenshot({ path: process.env.ACTIVITY_SHOT ?? "test-results/activity-bar.png" });

    await bar.getByRole("button", { name: /Nightly sales/ }).click();
    await expect(page.getByRole("dialog", { name: "Running jobs" })).toContainText("Stop");
    if (process.env.ACTIVITY_SHOT) await page.screenshot({ path: process.env.ACTIVITY_SHOT.replace(".png", "-open.png") });
    await page.keyboard.press("Escape");

    await emit(page, "job-run", { job_id: job.id, run: run("error", { status: "error", error: 'relation "orders" does not exist' }) });
    await expect(bar.getByRole("alert")).toContainText('Failed at load_orders: relation "orders" does not exist');
    await bar.getByRole("button", { name: "Dismiss Nightly sales" }).click();
    await expect(bar).toHaveCount(0);
  });
});
