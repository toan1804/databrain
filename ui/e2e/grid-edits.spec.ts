import { expect, test, type Page } from "@playwright/test";
import { backend, calls, emit, openApp, SHOP } from "./app";

// The result grid is a canvas: cells are reached by position (rowHeight 28,
// headerHeight 32 in ResultGrid.tsx, the row-number column first).
const HEADER = 32;
const ROW = 28;

const COLUMNS = ["customer_identifier", "customer_full_name"];
const ROWS = [
  ["1", "Alice Liddell"],
  ["2", "Bob Marley"],
  ["3", "Carol Danvers"],
  ["4", "Dan Brown"],
];

const TARGET = { connection_id: SHOP.id, schema: "public", table: "customers", key_columns: [0], columns: ["id", "name"], apply: "transaction" };

/** Another connection, never production. */
const SCRATCH = { ...SHOP, id: "conn-scratch", name: "Scratch", env: "dev" };

/** A tab on Shop (as `env`) showing one result of ROWS. */
async function showResult(page: Page, env = "dev") {
  await openApp(
    page,
    backend({
      list_connections: [{ ...SHOP, env }, SCRATCH],
      load_tabs: [{ id: "tab-1", title: "Query 1", sql: "select * from customers", connection_id: SHOP.id }],
      run_query: {
        status: "started",
        job_id: "job-1",
        statements: [{ index: 0, sql: "select * from customers", start: 0, end: 23, classification: { kind: "read", missing_where: false, keyword: "SELECT" } }],
      },
      fetch_page: { view_rows: ROWS.length, total_rows: ROWS.length, offset: 0, row_ids: ROWS.map((_, i) => i), rows: ROWS },
      edit_target: TARGET,
      edit_preview: [{ sql: "UPDATE public.customers SET name = 'Alicia' WHERE id = 1", expect_rows: 1 }],
      apply_edits: { rows: 1, in_open_transaction: false },
      copy_rows: "copied",
    }),
  );
  await page.getByRole("button", { name: "Run all" }).click();
  await emit(page, "job-event", {
    type: "statement_finished",
    job_id: "job-1",
    tab_id: "tab-1",
    index: 0,
    result: {
      id: "res-1",
      columns: COLUMNS.map((name) => ({ name, data_type: "Utf8", db_type: "text", family: "text" })),
      total_rows: ROWS.length,
      complete: true,
      truncated: false,
      bytes: 100,
    },
    output: {
      handle: "o1",
      name: null,
      version_of: null,
      result_id: "res-1",
      connection_id: SHOP.id,
      connection_name: SHOP.name,
      kind: "postgres",
      sql: "select * from customers",
      tab_id: "tab-1",
      statement_index: 0,
      created_at: Date.now(),
      rows: ROWS.length,
      columns: [],
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
    },
    rows_affected: null,
    duration_ms: 3,
    notices: [],
  });
  await emit(page, "job-event", { type: "job_finished", job_id: "job-1", tab_id: "tab-1", status: "success", duration_ms: 5, in_transaction: false });
  await expect(page.getByTestId("data-grid-canvas")).toBeVisible();
  // The first page is in (the footer counts it) before cells are clicked.
  await expect(page.getByText(`${ROWS.length} rows`, { exact: true })).toBeVisible();
  // React's dev mode mounts the grid twice, reading the first page twice (and
  // clearing the selection each time): wait for the last read.
  let seen = -1;
  await expect
    .poll(async () => {
      const n = (await calls(page, "fetch_page")).length;
      const still = n > 0 && n === seen;
      seen = n;
      return still;
    }, { intervals: [300] })
    .toBe(true);
}

/** Types `value` into the cell at (`col`, `row`) of a grid in edit mode. */
async function typeInto(page: Page, col: number, row: number, value: string) {
  const c = await cell(page, col, row);
  await page.mouse.dblclick(c.x, c.y);
  const input = page.locator(".gdg-input");
  await expect(input).toBeVisible();
  await input.fill(value);
  await input.press("Enter");
}

/** Waits for two frames: the grid's selection, a controlled prop, is applied before the next click reads it. */
async function settle(page: Page) {
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
}

/** A click on `at` with `key` held (Shift extends the block, {@link multiKey} adds one). */
async function clickWith(page: Page, key: string, at: { x: number; y: number }) {
  await page.keyboard.down(key);
  await page.mouse.click(at.x, at.y);
  await page.keyboard.up(key);
}

/** The key that adds a block: Cmd where the page says it runs on a Mac, Ctrl elsewhere (as glide reads it). */
async function multiKey(page: Page): Promise<string> {
  return (await page.evaluate(() => navigator.platform.toLowerCase().startsWith("mac"))) ? "Meta" : "Control";
}

/** Column widths as ResultGrid sizes them (`estimateWidth`), after the 32 px row-number column. */
const WIDTHS = COLUMNS.map((name, i) => {
  let chars = name.length + 3;
  for (const r of ROWS) chars = Math.max(chars, Math.min(r[i].length, 60));
  return Math.max(70, Math.min(420, Math.round(chars * 7.6 + 24)));
});

/** The middle of a data cell (`col`, `row` from 0) on the grid's canvas. */
async function cell(page: Page, col: number, row: number) {
  const box = (await page.getByTestId("data-grid-canvas").boundingBox())!;
  const left = 32 + WIDTHS.slice(0, col).reduce((a, b) => a + b, 0);
  return { x: box.x + left + WIDTHS[col] / 2, y: box.y + HEADER + ROW * row + ROW / 2 };
}

/** Edit mode on, then `value` typed into the first row's name. */
async function editName(page: Page, value: string) {
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  await expect(page.getByRole("button", { name: "Done editing" })).toBeVisible();
  await typeInto(page, 1, 0, value);
  await expect(page.getByText("· 1 change")).toBeVisible();
}

test.describe("result grid", () => {
  test("saving edits on production shows the statements and asks first", async ({ page }) => {
    await showResult(page, "prod");
    // The tab moves to a dev connection: the rows are still saved on Shop, so it still asks.
    await page.getByRole("combobox", { name: "Connection" }).selectOption({ label: "Scratch" });
    await editName(page, "Alicia");
    await page.getByRole("button", { name: "Save", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("Save 1 change on production?");
    await expect(dialog).toContainText('"Shop" is a production connection');
    await expect(dialog).toContainText("UPDATE public.customers SET name = 'Alicia' WHERE id = 1");
    // Nothing is sent before the click.
    expect(await calls(page, "apply_edits")).toHaveLength(0);

    // Refused: nothing saved, the edit is still there.
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toHaveCount(0);
    expect(await calls(page, "apply_edits")).toHaveLength(0);
    await expect(page.getByText("· 1 change")).toBeVisible();

    // Allowed: saved once, as confirmed.
    await page.getByRole("button", { name: "Save", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Save changes" }).click();
    await expect.poll(async () => (await calls(page, "apply_edits")).length).toBe(1);
    expect((await calls(page, "apply_edits"))[0]).toMatchObject({ resultId: "res-1", confirmed: true });
    await expect(page.getByText("double-click a cell to change it")).toBeVisible();
  });

  test("a server that asks for confirmation is asked back, and saved only on yes", async ({ page }) => {
    await showResult(page, "dev");
    // The connection became production on the server since the list was read.
    await page.evaluate(() => {
      const w = window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown> } };
      w.__DB_MOCK__.handlers.apply_edits = (a: { confirmed: boolean }) =>
        a.confirmed ? { rows: 1, in_open_transaction: false } : { __error: { kind: "invalid", code: "needs_confirmation", message: '"Shop" is a production connection: confirm the changes.' } };
    });
    await editName(page, "Alicia");

    // Refused: asked once, nothing saved again, the edit kept.
    await page.getByRole("button", { name: "Save", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText('"Shop" is a production connection: confirm the changes.');
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toHaveCount(0);
    expect((await calls(page, "apply_edits")).map((c) => c.confirmed)).toEqual([false]);
    await expect(page.getByText("· 1 change")).toBeVisible();

    // Allowed: sent again, confirmed.
    await page.getByRole("button", { name: "Save", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Save changes" }).click();
    await expect.poll(async () => (await calls(page, "apply_edits")).map((c) => c.confirmed)).toEqual([false, false, true]);
    await expect(page.getByText("double-click a cell to change it")).toBeVisible();
  });

  test("Backspace on a selection in edit mode changes nothing; typing does", async ({ page }) => {
    await showResult(page);
    await page.getByRole("button", { name: "Edit", exact: true }).click();
    await expect(page.getByText("double-click a cell to change it")).toBeVisible();
    // Select the first two rows' names and press Backspace, then Delete.
    const a = await cell(page, 1, 0);
    const b = await cell(page, 1, 1);
    await page.mouse.click(a.x, a.y);
    await settle(page);
    await clickWith(page, "Shift", b);
    await settle(page);
    await page.keyboard.press("Backspace");
    await page.keyboard.press("Delete");
    await expect(page.getByText("double-click a cell to change it")).toBeVisible();
    await expect(page.getByRole("button", { name: "Save", exact: true })).toBeDisabled();

    // A value typed into a cell is an edit.
    await typeInto(page, 1, 0, "Alicia");
    await expect(page.getByText("· 1 change")).toBeVisible();
  });

  test("right-click on an earlier block of a selection keeps the selection", async ({ page }) => {
    await showResult(page);
    // Two blocks: Bob's id, then Cmd/Ctrl-click adds Carol's name.
    const a1 = await cell(page, 0, 1);
    await page.mouse.click(a1.x, a1.y);
    await settle(page);
    const b = await cell(page, 1, 2);
    await clickWith(page, await multiKey(page), b);
    await settle(page);

    // Right-click on the earlier block: the selection is kept, so Copy says it is not one block.
    await page.mouse.click(a1.x, a1.y, { button: "right" });
    await page.getByRole("button", { name: /^Copy(?! with| as)/ }).click();
    await expect(page.getByText(/The selection is not one block/)).toBeVisible();
    expect(await calls(page, "copy_rows")).toHaveLength(0);

    // Right-click outside the selection: that cell alone is selected and copied.
    const out = await cell(page, 1, 0);
    await page.mouse.click(out.x, out.y, { button: "right" });
    await page.getByRole("button", { name: /^Copy(?! with| as)/ }).click();
    await expect.poll(async () => (await calls(page, "copy_rows")).length).toBe(1);
    expect((await calls(page, "copy_rows"))[0]).toMatchObject({ resultId: "res-1", offset: 0, limit: 1, options: { columns: [1] } });
  });

  test("the first page is read once: the quick filter settling to the same filter doesn't read it again", async ({ page }) => {
    await showResult(page);
    // Past the quick filter's 250 ms debounce; React's dev mode mounts the grid twice.
    await page.waitForTimeout(600);
    expect((await calls(page, "fetch_page")).filter((c) => c.offset === 0)).toHaveLength(2);
  });
});
