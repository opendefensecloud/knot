import { expect, test, type Locator, type Page } from "@playwright/test";

import { reset } from "../support/reset";

// Reset once: the owner is created in the beforeAll below, and a per-test
// reset would delete it out from under the later tests.
test.beforeAll(reset);

const EMAIL = "owner@table-editing.test";
const PASSWORD = "hunter22!tables";

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill(EMAIL);
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill(PASSWORD);
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
  await page.close();
});

async function newDoc(page: Page): Promise<Locator> {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
  await page.getByTestId("new-doc").click();
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
  return page.locator("[data-testid='editor-host'] .ProseMirror");
}

/**
 * "before", then a 3×3 table filled H1 H2 H3 / a1 a2 a3 / b1 b2 b3, typed the
 * way a person fills one: Tab from cell to cell.
 */
async function tableAfterParagraph(page: Page, prose: Locator) {
  await prose.click();
  await page.keyboard.type("before");
  await page.keyboard.press("Enter");
  await page.getByTestId("toolbar-table").click();
  const texts = ["H1", "H2", "H3", "a1", "a2", "a3", "b1", "b2", "b3"];
  for (const [i, text] of texts.entries()) {
    if (i > 0) await page.keyboard.press("Tab");
    // ProseMirror reads the DOM selection back asynchronously: typing right
    // after Tab moved cells could land in the cell it left.
    await expect
      .poll(() =>
        prose.evaluate((el) => {
          const { $from } = (el as unknown as { editor: { state: { selection: { $from: { index(d: number): number; depth: number } } } } }).editor.state.selection;
          return $from.index($from.depth - 3) * 3 + $from.index($from.depth - 2);
        }),
      )
      .toBe(i);
    await page.keyboard.type(text);
  }
  await expect(prose.locator("td").last()).toHaveText("b3");
}

/** The text of the cell the editor's caret is in; ProseMirror takes a click's
 *  selection asynchronously, so a paste right after one can land elsewhere. */
const caretCell = (prose: Locator) =>
  prose.evaluate((el) => {
    const { $from } = (el as unknown as { editor: { state: { selection: { $from: { node(d: number): { textContent: string }; depth: number } } } } }).editor.state.selection;
    return $from.node($from.depth - 1).textContent;
  });

const cell = (prose: Locator, text: string) => prose.locator("td, th").filter({ hasText: new RegExp(`^${text}$`) });

test("entering a table leaves the toolbar on one row and the page where it was", async ({ page }) => {
  const prose = await newDoc(page);
  await tableAfterParagraph(page, prose);
  const toolbar = page.getByTestId("editor-toolbar");

  await prose.getByText("before").click();
  await expect(page.getByTestId("toolbar-table")).toBeVisible();
  const outside = (await toolbar.boundingBox())!.height;
  const a1 = (await cell(prose, "a1").boundingBox())!.y;

  await cell(prose, "a1").click();
  await expect(page.getByTestId("toolbar-table-menu")).toBeVisible();
  expect((await toolbar.boundingBox())!.height).toBe(outside);
  expect((await cell(prose, "a1").boundingBox())!.y).toBe(a1);
});

test("dragging across two body cells selects those cells", async ({ page }) => {
  const prose = await newDoc(page);
  await tableAfterParagraph(page, prose);
  // Start from outside the table, as a person reaching for it does.
  await prose.getByText("before").click();

  const from = (await cell(prose, "a1").boundingBox())!;
  const to = (await cell(prose, "a2").boundingBox())!;
  await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
  await page.mouse.down();
  await page.mouse.move(to.x + to.width / 2, to.y + to.height / 2, { steps: 8 });
  await page.mouse.up();

  await expect(prose.locator(".selectedCell")).toHaveText(["a1", "a2"]);
  await page.keyboard.press("ControlOrMeta+b");
  await expect(prose.locator("td strong")).toHaveText(["a1", "a2"]);
  await expect(prose.locator("th strong")).toHaveCount(0);
});

test("a markdown table pasted into a cell fills the cells instead of nesting", async ({ page }) => {
  const prose = await newDoc(page);
  await tableAfterParagraph(page, prose);
  await cell(prose, "a1").click();
  await expect.poll(() => caretCell(prose)).toBe("a1");

  await prose.evaluate((el) => {
    const data = new DataTransfer();
    data.setData("text/plain", "| x | y |\n| --- | --- |\n| 1 | 2 |");
    el.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true }));
  });

  await expect(prose.locator("table table")).toHaveCount(0);
  // The pasted header row lands in the body: body cells there, not headers.
  await expect(prose.locator("tr").nth(1).locator("td")).toHaveText(["x", "y", "a3"]);
  await expect(prose.locator("tr").nth(2).locator("td")).toHaveText(["1", "2", "b3"]);
  await expect(prose.locator("th")).toHaveText(["H1", "H2", "H3"]);
});

test("ArrowDown from the last row of a table that ends the page opens a line below it", async ({ page }) => {
  const prose = await newDoc(page);
  await tableAfterParagraph(page, prose);

  // The caret is in the last cell after filling the table.
  await page.keyboard.press("ArrowDown");
  await page.keyboard.type("below");
  await expect(prose.locator(":scope > p").last()).toHaveText("below");
  await expect(cell(prose, "b3")).toBeVisible();
});

for (const gap of [8, 40]) {
  test(`a click ${gap}px below a table that ends the page opens a line there`, async ({ page }) => {
    const prose = await newDoc(page);
    await tableAfterParagraph(page, prose);
    await prose.getByText("before").click();

    const table = (await prose.locator("table").boundingBox())!;
    await page.mouse.click(table.x + 40, table.y + table.height + gap);
    await page.keyboard.type("clicked");
    await expect(prose.locator(":scope > p").last()).toHaveText("clicked");
    await expect(prose.locator(":scope > p").first()).toHaveText("before");
    await expect(cell(prose, "H1")).toBeVisible();
  });
}
