import { expect, test, type Locator, type Page } from "@playwright/test";

import { reset } from "../support/reset";

// Reset once: the owner is created in the beforeAll below, and a per-test
// reset would delete it out from under the later tests.
test.beforeAll(reset);

const EMAIL = "owner@dock.test";
const PASSWORD = "hunter22!dock";

test.beforeAll(async () => {
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD, display_name: "Owner" }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});

async function signIn(page: Page) {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
}

const PARAGRAPHS = 150;
const LONG_MARKDOWN = Array.from(
  { length: PARAGRAPHS },
  (_, i) =>
    `Paragraph ${i + 1}. Long documents are read top to bottom, and the controls must stay in reach ` +
    `without scrolling back up. This sentence pads the paragraph to about two lines of text.`,
).join("\n\n");

/** A new page holding a long document, left in view mode at the top. */
async function longDocInViewMode(page: Page, markdown = LONG_MARKDOWN, paragraphs = PARAGRAPHS) {
  await signIn(page);
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
  // On a phone the drawer stays open over the page after creating one.
  // Tap the backdrop's right-hand side: its centre is under the drawer.
  const backdrop = page.getByTestId("sidebar-backdrop");
  if ((await backdrop.count()) > 0) await backdrop.click({ position: { x: 350, y: 300 } });
  await expect(backdrop).toHaveCount(0);
  await page.getByTestId("doc-import-md-input").setInputFiles({
    name: "long.md",
    mimeType: "text/markdown",
    buffer: Buffer.from(markdown),
  });
  await expect(prose(page).locator("> p")).toHaveCount(paragraphs, { timeout: 10_000 });
  const toggle = page.getByTestId("toggle-edit-mode");
  if ((await toggle.getAttribute("aria-pressed")) === "true") await toggle.click();
  await expect(toggle).toHaveAttribute("aria-pressed", "false");
}

function prose(page: Page): Locator {
  return page.locator("[data-testid='editor-host'] .ProseMirror");
}

function paragraph(page: Page, n: number): Locator {
  return prose(page).locator(`> p:nth-child(${n})`);
}

async function scrollMid(page: Page) {
  await page.evaluate(() => {
    const main = document.querySelector("main")!;
    main.scrollTop = Math.round(main.scrollHeight / 2);
  });
  await expect(page.getByTestId("doc-dock")).toBeInViewport();
}

async function top(locator: Locator): Promise<number> {
  return (await locator.boundingBox())!.y;
}

/** The 1-based index of the first paragraph whose top is below `y`. */
async function firstParagraphBelow(page: Page, y: number): Promise<number> {
  return prose(page).evaluate((el, line) => {
    const blocks = Array.from(el.children);
    return blocks.findIndex((b) => b.getBoundingClientRect().top >= line) + 1;
  }, y);
}

/** Viewport rect of the caret, or null without a selection. */
async function caretRect(page: Page): Promise<{ top: number; bottom: number } | null> {
  return page.evaluate(() => {
    const selection = window.getSelection();
    if (!selection || selection.rangeCount === 0) return null;
    const range = selection.getRangeAt(0).cloneRange();
    // A collapsed range at a text start can measure as empty; widen it.
    if (range.collapsed && range.startContainer.nodeType === Node.TEXT_NODE) {
      const text = range.startContainer as Text;
      if (range.startOffset < text.length) range.setEnd(text, range.startOffset + 1);
      else if (range.startOffset > 0) range.setStart(text, range.startOffset - 1);
    }
    const rect = range.getBoundingClientRect();
    return { top: rect.top, bottom: rect.bottom };
  });
}

async function scrollTop(page: Page): Promise<number> {
  return page.evaluate(() => document.querySelector("main")!.scrollTop);
}

/** How far the window itself could scroll down: the root's scrollable
 *  overflow past the viewport. Zero or less means the window has no
 *  scrollbar of its own and only the panes inside it scroll. */
async function windowOverflow(page: Page): Promise<number> {
  return page.evaluate(() => {
    const root = document.scrollingElement!;
    return root.scrollHeight - root.clientHeight;
  });
}

/** The window has nothing to scroll, here and in every state the test put
 *  the page in. Asks it to scroll anyway, so a page whose overflow only
 *  shows up under a scroll fails here too. scrollTo rather than the mouse
 *  wheel: a wheel lands in <main> first and reaches the window only once
 *  main has run out. */
async function expectWindowNeverScrolls(page: Page) {
  expect(await windowOverflow(page)).toBeLessThanOrEqual(0);
  await page.evaluate(() => window.scrollTo(0, 1e6));
  expect(await page.evaluate(() => window.scrollY)).toBe(0);
}

/** #87: the edit-mode announcer, a visually hidden live region at the foot of
 *  the document, once escaped <main>'s scrolling and gave the window a third
 *  scrollbar the length of the document, over the sidebar's and the
 *  content's own. It is there in view mode too, and gets its text on the
 *  switch to edit mode.
 *
 *  The window checks are the user-visible result, and the shell's
 *  overflow-clip backstop alone would also pass them. What pins the fix
 *  itself is the first check: the announcer resolves against <main> (its
 *  offsetParent, the nearest positioned ancestor of a positioned element), so
 *  <main> scrolls and clips it. AppShell.test.tsx pins the classes too. */
async function expectOnlyThePanesScroll(page: Page) {
  await expect(page.getByTestId("edit-mode-announcer")).toHaveCount(1);
  expect(
    await page
      .getByTestId("edit-mode-announcer")
      .evaluate((announcer) => (announcer as HTMLElement).offsetParent?.tagName),
  ).toBe("MAIN");
  expect(await windowOverflow(page)).toBeLessThanOrEqual(0);

  await scrollMid(page);
  expect(await windowOverflow(page)).toBeLessThanOrEqual(0);

  await page.getByTestId("dock-toggle-edit-mode").click();
  await expect(page.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByTestId("edit-mode-announcer")).toHaveText("Editing");
  await expectWindowNeverScrolls(page);
  // The document itself still scrolls, inside <main>.
  expect(await scrollTop(page)).toBeGreaterThan(0);
}

test("a long document scrolls inside <main>; the window itself never scrolls (#87)", async ({ page }) => {
  await longDocInViewMode(page);
  await expectOnlyThePanesScroll(page);
});

test("mid-document, the dock enters and leaves edit mode without moving the text", async ({ page }) => {
  await longDocInViewMode(page);
  await scrollMid(page);
  await expect(page.getByTestId("toggle-edit-mode")).not.toBeInViewport();

  const dock = page.getByTestId("doc-dock");
  const dockBox = (await dock.boundingBox())!;
  const n = await firstParagraphBelow(page, dockBox.y + dockBox.height);
  expect(n).toBeGreaterThan(1);
  const before = await top(paragraph(page, n));

  await dock.getByTestId("dock-toggle-edit-mode").click();
  await expect(page.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByTestId("editor-toolbar")).toBeInViewport();
  await expect(page.getByTestId("toolbar-doc-controls")).toBeVisible();
  await expect(page.getByTestId("doc-dock")).toHaveCount(0);
  expect(Math.abs((await top(paragraph(page, n))) - before)).toBeLessThanOrEqual(2);

  // The caret sits on the first line the reader can see below the toolbar,
  // and typing there does not scroll the page.
  const toolbarBox = (await page.getByTestId("editor-toolbar").boundingBox())!;
  const toolbarBottom = toolbarBox.y + toolbarBox.height;
  const caret = (await caretRect(page))!;
  expect(caret.top).toBeGreaterThanOrEqual(toolbarBottom - 1);
  expect(caret.top).toBeLessThan(toolbarBottom + 40);
  const scrolledTo = await scrollTop(page);
  await page.keyboard.type("Z");
  await expect(prose(page)).toContainText("Z");
  expect(Math.abs((await scrollTop(page)) - scrolledTo)).toBeLessThanOrEqual(2);

  // ⌘E while typing leaves edit mode and no longer toggles inline code.
  await page.keyboard.press("ControlOrMeta+e");
  await expect(page.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-pressed", "false");
  await expect(prose(page).locator("code")).toHaveCount(0);
  expect(Math.abs((await top(paragraph(page, n))) - before)).toBeLessThanOrEqual(2);
  await expect(page.getByTestId("doc-dock")).toBeInViewport();
  await expect(page.getByTestId("dock-toggle-edit-mode")).toBeFocused();
});

test("a click in the text before ⌘E keeps the caret at the click", async ({ page }) => {
  await longDocInViewMode(page);
  await scrollMid(page);
  const dockBox = (await page.getByTestId("doc-dock").boundingBox())!;
  const n = (await firstParagraphBelow(page, dockBox.y + dockBox.height)) + 5;

  await paragraph(page, n).click({ position: { x: 4, y: 4 } });
  await page.keyboard.press("ControlOrMeta+e");
  await expect(page.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-pressed", "true");
  await page.keyboard.type("Q");
  await expect(paragraph(page, n)).toContainText("Q");
});

test("a caret placed in a table stays clear of the toolbar", async ({ page }) => {
  // The toolbar swaps Insert table for the table menu once the caret is in a
  // table; the caret's line must not end up under the toolbar.
  const blocks = LONG_MARKDOWN.split("\n\n");
  const table = ["| Item | Notes |", "| --- | --- |", ...Array.from({ length: 12 }, (_, i) => `| Row ${i + 1} | Cell text ${i + 1} |`)].join("\n");
  const markdown = [...blocks.slice(0, 80), table, ...blocks.slice(80)].join("\n\n");
  await longDocInViewMode(page, markdown, PARAGRAPHS);

  // Bring the table's first rows just under the dock.
  await page.evaluate(() => {
    const main = document.querySelector("main")!;
    const tableEl = document.querySelector(".ProseMirror table")!;
    main.scrollTop += tableEl.getBoundingClientRect().top - 60;
  });
  await expect(page.getByTestId("doc-dock")).toBeInViewport();
  await page.getByTestId("dock-toggle-edit-mode").click();
  await expect(page.getByTestId("toolbar-table-menu")).toBeVisible();
  // Let the reveal frame run after the toolbar's re-render.
  await page.waitForTimeout(100);

  const toolbarBox = (await page.getByTestId("editor-toolbar").boundingBox())!;
  const caret = (await caretRect(page))!;
  expect(caret.top).toBeGreaterThanOrEqual(toolbarBox.y + toolbarBox.height - 1);
});

test("the add-comment button clears the dock", async ({ page }) => {
  await longDocInViewMode(page);
  await scrollMid(page);
  const dockBox = (await page.getByTestId("doc-dock").boundingBox())!;
  const n = await firstParagraphBelow(page, dockBox.y + dockBox.height);

  await paragraph(page, n).dblclick({ position: { x: 20, y: 6 } });
  const float = page.getByTestId("add-comment-float");
  await expect(float).toBeVisible();
  expect((await float.boundingBox())!.y).toBeGreaterThanOrEqual(dockBox.y + dockBox.height - 1);
});

test("opening and closing the comment rail keeps the reading position", async ({ page }) => {
  await longDocInViewMode(page);
  await scrollMid(page);
  const dockBox = (await page.getByTestId("doc-dock").boundingBox())!;
  const n = await firstParagraphBelow(page, dockBox.y + dockBox.height);
  const before = await top(paragraph(page, n));

  const comments = page.getByTestId("dock-open-comments");
  await comments.click();
  await expect(comments).toHaveAttribute("aria-pressed", "true");
  await page.waitForTimeout(400);
  expect(Math.abs((await top(paragraph(page, n))) - before)).toBeLessThanOrEqual(2);

  await comments.click();
  await expect(comments).toHaveAttribute("aria-pressed", "false");
  await page.waitForTimeout(400);
  expect(Math.abs((await top(paragraph(page, n))) - before)).toBeLessThanOrEqual(2);
});

test("at the top the toolbar stays one row and the dock goes away", async ({ page }) => {
  await longDocInViewMode(page);
  await page.getByTestId("toggle-edit-mode").click();
  const toolbar = page.getByTestId("editor-toolbar");
  await expect(toolbar).toBeVisible();
  // Click the first paragraph so no table controls widen the row.
  await paragraph(page, 1).click();
  expect((await toolbar.boundingBox())!.height).toBeLessThanOrEqual(50);
  await expect(page.getByTestId("toolbar-doc-controls")).toBeHidden();

  await page.getByTestId("toggle-edit-mode").click();
  await scrollMid(page);
  await page.evaluate(() => { document.querySelector("main")!.scrollTop = 0; });
  await expect(page.getByTestId("doc-dock")).toHaveCount(0);
});

test("at the top, the shortcut puts the caret in the text, and End stays on the line", async ({ page }) => {
  // #86: entering edit mode at the top left focus outside the editor, so
  // End scrolled <main> to the bottom instead of moving the caret. Lines that
  // fit on one row each, so the end of the caret's line is the end of its
  // block. ControlOrMeta is the toggle's Mod: ⌘ on a Mac, Ctrl elsewhere.
  const shortLines = Array.from({ length: PARAGRAPHS }, (_, i) => `Line ${i + 1} fits on one row.`).join("\n\n");
  await longDocInViewMode(page, shortLines, PARAGRAPHS);
  const startScrollTop = await scrollTop(page);

  await page.keyboard.press("ControlOrMeta+e");
  await expect(page.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-pressed", "true");
  await expect(prose(page)).toBeFocused();
  // Focused without scrolling.
  expect(Math.abs((await scrollTop(page)) - startScrollTop)).toBeLessThanOrEqual(2);

  // The top-level block holding the caret: its 1-based index and text.
  const caretBlock = await prose(page).evaluate((root) => {
    const focusNode = window.getSelection()?.focusNode ?? null;
    const focusElement = focusNode?.nodeType === Node.TEXT_NODE ? focusNode.parentElement : (focusNode as Element | null);
    const block = Array.from(root.children).find((child) => child.contains(focusElement));
    return block ? { index: Array.from(root.children).indexOf(block) + 1, text: block.textContent ?? "" } : null;
  });
  expect(caretBlock).not.toBeNull();

  const beforeEnd = await scrollTop(page);
  await page.keyboard.press("End");
  expect(Math.abs((await scrollTop(page)) - beforeEnd)).toBeLessThanOrEqual(2);
  await page.keyboard.type("X");
  await expect(prose(page).locator(`> :nth-child(${caretBlock!.index})`)).toHaveText(`${caretBlock!.text}X`);
  expect(Math.abs((await scrollTop(page)) - beforeEnd)).toBeLessThanOrEqual(2);
});

test.describe("on a phone", () => {
  test.use({ viewport: { width: 375, height: 667 } });

  test("the dock and toolbar stay clear of the menu button and never scroll sideways", async ({ page }) => {
    await longDocInViewMode(page);
    await scrollMid(page);

    const menu = (await page.getByTestId("menu-toggle").boundingBox())!;
    const edit = (await page.getByTestId("dock-toggle-edit-mode").boundingBox())!;
    expect(edit.x).toBeGreaterThanOrEqual(menu.x + menu.width);
    expect(
      await page.evaluate(() => {
        const main = document.querySelector("main")!;
        return main.scrollWidth - main.clientWidth;
      }),
    ).toBeLessThanOrEqual(1);

    // The toolbar wraps to several rows here, taller than the dock: the
    // paragraph being read moves down to stay visible below it.
    const dockBox = (await page.getByTestId("doc-dock").boundingBox())!;
    const n = await firstParagraphBelow(page, dockBox.y + dockBox.height);
    await page.getByTestId("dock-toggle-edit-mode").click();
    const bold = (await page.getByTestId("toolbar-bold").boundingBox())!;
    expect(bold.x).toBeGreaterThanOrEqual(menu.x + menu.width);
    const toolbarBox = (await page.getByTestId("editor-toolbar").boundingBox())!;
    expect(await top(paragraph(page, n))).toBeGreaterThanOrEqual(toolbarBox.y + toolbarBox.height - 1);
  });

  test("a long document scrolls inside <main>; the window itself never scrolls (#87)", async ({ page }) => {
    // The phone shell is a plain block, not a grid, and <main> is its own
    // h-dvh scroller there: the announcer escaped it the same way.
    await longDocInViewMode(page);
    await expectOnlyThePanesScroll(page);
  });
});
