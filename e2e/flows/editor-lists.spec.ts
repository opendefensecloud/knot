import { expect, test, type Locator, type Page } from "@playwright/test";

import { reset } from "../support/reset";

/**
 * Nesting list items in a real browser (#78, "nesting of bullet points does
 * not work, they are all on top level").
 *
 * Every way of nesting an item is checked all the way through: the editor's
 * DOM, the Markdown the server exports from the shared document, and the
 * document as it comes back after a reload. Tab, a typed indent ("  - "),
 * the toolbar's Indent button, and Tab in a list inside a table cell.
 */

test.beforeAll(reset);

const EMAIL = "owner@editor-lists.test";
const PASSWORD = "hunter22!lists";

test.beforeAll(async () => {
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD, display_name: "Owner" }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});

async function connected(page: Page) {
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 15_000,
  });
}

/** Signs in, opens a new blank doc, and returns its id and editor. */
async function newDoc(page: Page): Promise<{ docId: string; prose: Locator }> {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await connected(page);
  const docId = page.url().match(/\/doc\/([^/?#]+)/)?.[1] ?? "";
  return { docId, prose: page.locator("[data-testid='editor-host'] .ProseMirror") };
}

/** "two" nested under "one": in the editor, in the export, and after a reload. */
async function expectNested(page: Page, docId: string, prose: Locator) {
  await expect(prose.locator("ul > li > ul > li")).toHaveText("two");
  await expect(prose.locator("ul > li > ul > li")).toHaveCount(1);

  // The server exports from the shared Y.Doc, so this proves the nesting
  // reached it rather than living only in this editor.
  await expect
    .poll(async () => (await page.request.get(`/api/docs/${docId}/markdown`)).text(), {
      timeout: 15_000,
    })
    .toContain("- one\n  - two");

  await page.reload();
  await connected(page);
  await expect(prose.locator("ul > li > ul > li")).toHaveText("two");
}

test("Tab nests a list item", async ({ page }) => {
  const { docId, prose } = await newDoc(page);
  await prose.click();
  await page.keyboard.type("- one");
  await page.keyboard.press("Enter");
  await page.keyboard.press("Tab");
  await page.keyboard.type("two");

  await expectNested(page, docId, prose);
});

test('typing "  - " at the start of an item nests it', async ({ page }) => {
  const { docId, prose } = await newDoc(page);
  await prose.click();
  await page.keyboard.type("- one");
  await page.keyboard.press("Enter");
  await page.keyboard.type("  - two");

  // The marker was consumed, not left in the text.
  await expect(prose).not.toContainText("- two");
  await expectNested(page, docId, prose);
});

test('the same "  - " indent on every nested line keeps those items siblings', async ({ page }) => {
  const { docId, prose } = await newDoc(page);
  await prose.click();
  await page.keyboard.type("- one");
  await page.keyboard.press("Enter");
  await page.keyboard.type("  - two");
  await page.keyboard.press("Enter");
  await page.keyboard.type("  - three");

  await expect(prose.locator("ul > li > ul > li")).toHaveText(["two", "three"]);
  await expect(prose.locator("ul ul ul")).toHaveCount(0);
  await expect
    .poll(async () => (await page.request.get(`/api/docs/${docId}/markdown`)).text(), {
      timeout: 15_000,
    })
    .toContain("- one\n  - two\n  - three");
});

test("the toolbar's Indent button nests a list item", async ({ page }) => {
  const { docId, prose } = await newDoc(page);
  await prose.click();
  await page.keyboard.type("- one");
  await page.keyboard.press("Enter");
  await page.keyboard.type("two");

  const toolbar = page.getByTestId("editor-toolbar");
  const indent = page.getByTestId("toolbar-indent");
  await expect(indent).toBeEnabled();
  // Indent and Outdent take the diagram buttons' slot: the toolbar stays on
  // one row (see doc-dock.spec.ts) with the caret in a list.
  expect((await toolbar.boundingBox())!.height).toBeLessThanOrEqual(50);

  await indent.click();
  await expect(indent).toBeDisabled();

  await expectNested(page, docId, prose);
});

test("Tab nests a list item inside a table cell", async ({ page }) => {
  const { prose } = await newDoc(page);
  await prose.click();
  await page.keyboard.type("before");
  await page.keyboard.press("Enter");
  await page.getByTestId("toolbar-table").click();
  await expect(prose.locator("td")).toHaveCount(6);

  await prose.locator("td").first().click();
  // Inserting the table left the caret in the first header cell, and
  // ProseMirror reads a click's selection back asynchronously (see
  // table-editing.spec.ts): typing straight away could land in the header.
  // Wait until the caret is in body row 1, cell 0: paragraph, cell, row, table.
  await expect
    .poll(() =>
      prose.evaluate((el) => {
        const { $from } = (el as unknown as { editor: { state: { selection: { $from: { index(d: number): number; depth: number } } } } }).editor.state.selection;
        return [$from.index($from.depth - 3), $from.index($from.depth - 2)];
      }),
    )
    .toEqual([1, 0]);
  await page.keyboard.type("- one");
  await page.keyboard.press("Enter");
  await page.keyboard.press("Tab");
  await page.keyboard.type("two");

  const nested = prose.locator("td").first().locator("ul > li > ul > li");
  await expect(nested).toHaveText("two");
  // Tab indented rather than moving on to the next cell.
  await expect(prose.locator("td").nth(1)).toHaveText("");

  await page.reload();
  await connected(page);
  await expect(prose.locator("td").first().locator("ul > li > ul > li")).toHaveText("two");
});
