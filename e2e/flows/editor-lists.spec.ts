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

/**
 * Numbered-list markers past item 9 (#95, '"10." shows as ".0."').
 *
 * A marker hangs outside its item, in the list's left padding, and the editor
 * host (`overflow-x: clip`) cuts off anything past its left edge. Its
 * `overflow-clip-margin` does not help: Chromium applies it only when both
 * axes clip. Typing the tenth item has to widen that padding enough for "10."
 * in every skin's font (the terminal skin's monospace one needed more even
 * for "1.").
 *
 * There is no layout API for a ::marker box. An outside marker ends where its
 * item's content begins, so its left edge is the item's left minus the width
 * of the marker text, measured with a probe span set in the marker's font.
 */
test("numbered-list markers from item 10 on stay inside the editor", async ({ page }) => {
  const { prose } = await newDoc(page);
  await prose.click();
  await page.keyboard.type("1. item 1");
  for (let itemNumber = 2; itemNumber <= 10; itemNumber++) {
    await page.keyboard.press("Enter");
    await page.keyboard.type(`item ${itemNumber}`);
  }
  await expect(prose.locator("ol > li")).toHaveCount(10);

  for (const skin of ["light", "paper", "terminal"]) {
    const room = await prose.evaluate(async (editorElement, skinId) => {
      document.documentElement.setAttribute("data-skin", skinId);
      // A face is only fetched once something uses it. Laying out under the
      // new skin starts that fetch; measuring before it lands would measure
      // the fallback font instead of the skin's.
      editorElement.getBoundingClientRect();
      await document.fonts.ready;
      const clipEdge = editorElement.closest("[data-testid='editor-host']")!.getBoundingClientRect().left;
      const items = Array.from(editorElement.querySelectorAll("ol > li"));
      return items.map((item, index) => {
        const probe = document.createElement("span");
        probe.textContent = `${index + 1}. `;
        probe.style.whiteSpace = "pre";
        probe.style.fontVariantNumeric = getComputedStyle(item, "::marker").fontVariantNumeric;
        item.appendChild(probe);
        const markerWidth = probe.getBoundingClientRect().width;
        probe.remove();
        return item.getBoundingClientRect().left - markerWidth - clipEdge;
      });
    }, skin);
    // Pixels between each marker's left edge and the clip: never negative.
    const roomText = room.map((pixels) => pixels.toFixed(1)).join(", ");
    expect(room.every((pixels) => pixels >= 0), `${skin}: ${roomText}`).toBe(true);
  }
});

/**
 * The wider gutter belongs to the long list alone. Custom properties inherit,
 * so a short numbered list nested in a long one could take the long list's
 * digit count, and every such sublist would shift right the moment the outer
 * list reached item 10.
 */
test("a short list nested in a long numbered list keeps the narrow gutter", async ({ page }) => {
  const { prose } = await newDoc(page);
  const paragraph = (text: string) => ({ type: "paragraph", content: [{ type: "text", text }] });
  const shortList = (label: string) => ({
    type: "ordered_list",
    content: [1, 2].map((n) => ({ type: "list_item", content: [paragraph(`${label} ${n}`)] })),
  });
  const longList = {
    type: "ordered_list",
    content: Array.from({ length: 10 }, (_, index) => ({
      type: "list_item",
      content: index === 0 ? [paragraph("item 1"), shortList("nested")] : [paragraph(`item ${index + 1}`)],
    })),
  };
  await prose.evaluate((el, content) => {
    (el as unknown as { editor: { commands: { setContent(c: unknown): void } } }).editor.commands.setContent({
      type: "doc",
      content,
    });
  }, [longList, shortList("separate")]);
  await expect(prose.locator(":scope > ol")).toHaveCount(2);

  const padding = (list: Locator) => list.evaluate((el) => parseFloat(getComputedStyle(el).paddingLeft));
  const outer = await padding(prose.locator(":scope > ol").first());
  const nested = await padding(prose.locator(":scope > ol > li > ol"));
  const separate = await padding(prose.locator(":scope > ol").nth(1));
  expect(nested).toBe(separate);
  expect(outer).toBeGreaterThan(separate);
});
