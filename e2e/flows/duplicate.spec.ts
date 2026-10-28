// Duplicate: "Duplicate…" in a page's sidebar menu puts a copy right after
// the page and opens it. A page with subpages first asks whether they go too
// (unticked by default). The copy has its own files and drawings and keeps
// the column widths, so it still works once the original is archived, and
// even once that is deleted for good.
import { expect, test, type Browser, type Locator, type Page } from "@playwright/test";

import { reset } from "../support/reset";

import { outline, seedPages, setupOwner, showTree, writeHeaders } from "./tree-helpers";

// Per test, not once: both tests sign up through /setup, which closes as
// soon as a user exists.
test.beforeEach(reset);

// 69-byte transparent 1x1 PNG.
const TINY_PNG_HEX =
  "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d4944415478da6360600000000004000" +
  "15c5b66e30000000049454e44ae426082";

const EDITOR = "[data-testid='editor-host'] .ProseMirror";

async function waitConnected(page: Page): Promise<void> {
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
}

/** The sidebar's page rows, top to bottom. */
function rows(page: Page): Locator {
  return page.locator("[data-testid^='doc-row-']");
}

/**
 * Copies page `id` from its sidebar menu. `inDialog` answers the dialog that
 * a page with subpages shows first. Without it, the copy must be made at
 * once, with no dialog in between. Returns the copy's id once the copy is
 * open, and the body the page sent.
 */
async function duplicateFromMenu(
  page: Page,
  id: string,
  inDialog?: (dialog: Locator) => Promise<void>,
): Promise<{ copy: string; sent: unknown }> {
  const answer = page.waitForResponse(
    (r) => r.request().method() === "POST" && new URL(r.url()).pathname === `/api/docs/${id}/duplicate`,
  );
  await page.getByTestId(`doc-row-${id}`).click({ button: "right" });
  await page.getByTestId("ctx-duplicate").click();
  if (inDialog) await inDialog(page.getByTestId("duplicate-dialog"));
  const res = await answer;
  expect(res.status()).toBe(201);
  const copy = ((await res.json()) as { id: string }).id;
  await page.waitForURL(new RegExp(`/doc/${copy}$`));
  await waitConnected(page);
  return { copy, sent: res.request().postDataJSON() };
}

test("duplicate_without_subpages", async ({ page }) => {
  await setupOwner(page);
  const ids = await seedPages(page.request, [
    { title: "Alpha" },
    { title: "Bravo" },
    { title: "Bravo notes", parent: "Bravo" },
    { title: "Charlie" },
    { title: "Delta" },
  ]);
  const md = await page.request.post(`/api/docs/${ids.Charlie}/markdown`, {
    headers: { ...(await writeHeaders(page.request)), "Content-Type": "text/markdown" },
    data: "Agenda for Charlie\n",
  });
  expect(md.status()).toBe(204);
  await showTree(page, Object.values(ids));

  // A page without subpages is copied at once: there is nothing to ask.
  const charlie = await duplicateFromMenu(page, ids.Charlie);
  expect(charlie.sent).toEqual({ include_subpages: false });
  await expect(page.getByTestId("duplicate-dialog")).toHaveCount(0);
  await expect(page.getByTestId("doc-title")).toHaveValue("Charlie (copy)");
  await expect(page.locator(EDITOR)).toContainText("Agenda for Charlie");
  await expect(page.getByTestId(`doc-row-${charlie.copy}`)).toHaveText("Charlie (copy)");
  await expect(rows(page)).toHaveText(["Alpha", "Bravo", "Bravo notes", "Charlie", "Charlie (copy)", "Delta"]);
  expect(await outline(page.request)).toEqual([
    "Alpha",
    "Bravo",
    "Bravo/Bravo notes",
    "Charlie",
    "Charlie (copy)",
    "Delta",
  ]);

  // A page with subpages asks first. Left unticked, they stay behind.
  const bravo = await duplicateFromMenu(page, ids.Bravo, async (dialog) => {
    await expect(dialog.getByRole("heading")).toHaveText("Duplicate “Bravo”");
    await expect(dialog.getByRole("checkbox", { name: "Include its 1 subpage", exact: true })).not.toBeChecked();
    await dialog.getByTestId("duplicate-confirm").click();
  });
  expect(bravo.sent).toEqual({ include_subpages: false });
  await expect(page.getByTestId("doc-title")).toHaveValue("Bravo (copy)");
  await expect(page.getByTestId(`doc-row-${bravo.copy}`)).toHaveText("Bravo (copy)");
  await expect(rows(page)).toHaveText([
    "Alpha",
    "Bravo",
    "Bravo notes",
    "Bravo (copy)",
    "Charlie",
    "Charlie (copy)",
    "Delta",
  ]);
  expect(await outline(page.request)).toEqual([
    "Alpha",
    "Bravo",
    "Bravo/Bravo notes",
    "Bravo (copy)",
    "Charlie",
    "Charlie (copy)",
    "Delta",
  ]);
});

/**
 * Drives a rectangle into the open Excalidraw modal through the dev-only
 * `window.__excalidrawAPI` and `window.__excalidrawConvert` hooks (see
 * excalidraw.spec.ts).
 */
async function drawRectangle(page: Page): Promise<void> {
  await page.evaluate(() => {
    const w = window as unknown as {
      __excalidrawAPI?: {
        getSceneElements: () => readonly unknown[];
        updateScene: (s: { elements: readonly unknown[] }) => void;
      };
      __excalidrawConvert?: (skeletons: ReadonlyArray<Record<string, unknown>>) => readonly unknown[];
    };
    const api = w.__excalidrawAPI;
    const convert = w.__excalidrawConvert;
    if (!api || !convert) throw new Error("excalidraw test hooks not ready");
    const built = convert([{ type: "rectangle", x: 100, y: 100, width: 120, height: 80 }]);
    api.updateScene({ elements: [...api.getSceneElements(), ...built] });
  });
}

/** Opens the page's board and waits until Excalidraw is ready and its
 *  connection is up. */
async function openBoard(page: Page): Promise<void> {
  await page.getByTestId("excalidraw-board-open").click();
  const modal = page.getByTestId("excalidraw-modal");
  await expect(modal).toBeVisible({ timeout: 10_000 });
  await page.waitForFunction(
    () => {
      const w = window as unknown as { __excalidrawAPI?: unknown; __excalidrawConvert?: unknown };
      return !!w.__excalidrawAPI && !!w.__excalidrawConvert;
    },
    null,
    { timeout: 15_000 },
  );
  await expect(modal.getByTestId("sync-status")).toHaveAttribute("data-status", "connected");
}

/** How many elements the open board shows. */
function boardElements(page: Page): Promise<number> {
  return page.evaluate(() => {
    const w = window as unknown as { __excalidrawAPI?: { getSceneElements: () => readonly unknown[] } };
    return w.__excalidrawAPI?.getSceneElements().length ?? 0;
  });
}

/** The table's column widths as the page lays them out, as shares of the
 *  table (`<col>` widths, from the stored weights). */
function columnShares(page: Page): Promise<string[]> {
  return page
    .locator(`${EDITOR} table colgroup col`)
    .evaluateAll((cols) => cols.map((col) => (col as HTMLElement).style.width));
}

/** The table's header cells' rendered widths, in px. */
function headerWidths(page: Page): Promise<number[]> {
  return page
    .locator(`${EDITOR} tr`)
    .first()
    .locator("th")
    .evaluateAll((cells) => cells.map((cell) => cell.getBoundingClientRect().width));
}

/**
 * Waits until the open page shows its image (loaded, not broken), its
 * board's preview and its table with the columns at `shares`. Returns the
 * image's src and the header cells' widths.
 */
async function expectContentShown(page: Page, shares: string[]): Promise<{ src: string; widths: number[] }> {
  const img = page.locator(`${EDITOR} img`).first();
  await expect(img).toBeVisible({ timeout: 10_000 });
  await expect
    .poll(() => img.evaluate((el) => (el as HTMLImageElement).complete && (el as HTMLImageElement).naturalWidth > 0))
    .toBe(true);
  await expect(page.locator("[data-testid='excalidraw-board'] svg").first()).toBeVisible({ timeout: 10_000 });
  await expect.poll(() => columnShares(page)).toEqual(shares);
  const src = await img.getAttribute("src");
  expect(src).toMatch(/^\/api\/blobs\/[0-9a-f-]{36}$/);
  return { src: src!, widths: await headerWidths(page) };
}

/**
 * Opens `path` in a browser context of its own, signed in as `page` is, and
 * runs `check` there. Its HTTP cache starts empty: what it shows comes from
 * the server, not from files `page` fetched before (blobs are cached for a
 * minute, board previews for 10 s).
 */
async function inFreshBrowser<T>(
  browser: Browser,
  page: Page,
  path: string,
  check: (fresh: Page) => Promise<T>,
): Promise<T> {
  const context = await browser.newContext({ storageState: await page.context().storageState() });
  try {
    const fresh = await context.newPage();
    await fresh.goto(path);
    await waitConnected(fresh);
    return await check(fresh);
  } finally {
    await context.close();
  }
}

type Board = { id: string };

async function boardsOf(page: Page, docId: string): Promise<Board[]> {
  const res = await page.request.get(`/api/docs/${docId}/boards`);
  expect(res.status()).toBe(200);
  return (await res.json()) as Board[];
}

test("duplicate_with_subpages", async ({ page, browser }) => {
  // Two Excalidraw loads and three browser contexts: on a cold dev server,
  // more than the default 30 s.
  test.slow();
  await setupOwner(page);
  const ids = await seedPages(page.request, [
    { title: "Handbook" },
    { title: "Chapter", parent: "Handbook" },
    { title: "Section", parent: "Chapter" },
    { title: "Appendix" },
  ]);
  await showTree(page, Object.values(ids), `/doc/${ids.Handbook}`);
  await waitConnected(page);

  // The page gets a paragraph, then each block on a new line right after
  // it: a table with its first column made wider, an image and a board.
  const prose = page.locator(EDITOR);
  await prose.click();
  await page.keyboard.type("Handbook intro");
  const newLineAfterIntro = async () => {
    await prose.getByText("Handbook intro").click();
    await page.keyboard.press("End");
    await page.keyboard.press("Enter");
  };

  await newLineAfterIntro();
  await page.getByTestId("toolbar-table").click();
  const firstHeader = prose.locator("tr").first().locator("th").first();
  await expect(firstHeader).toBeVisible();
  const before = (await firstHeader.boundingBox())!.width;
  const handle = (await prose.locator('.knot-col-handle[data-border="0"]').boundingBox())!;
  const x = handle.x + handle.width / 2;
  const y = handle.y + handle.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + 120, y, { steps: 5 });
  await page.mouse.up();
  await expect.poll(async () => (await firstHeader.boundingBox())!.width).toBeGreaterThan(before + 100);
  const shares = await columnShares(page);

  await newLineAfterIntro();
  await page.getByTestId("toolbar-attachment-input").setInputFiles({
    name: "tiny.png",
    mimeType: "image/png",
    buffer: Buffer.from(TINY_PNG_HEX, "hex"),
  });
  await expect(prose.locator("img")).toBeVisible({ timeout: 8_000 });

  await newLineAfterIntro();
  await page.getByTestId("toolbar-excalidraw").click();
  await expect(page.getByTestId("excalidraw-board")).toBeVisible({ timeout: 8_000 });
  await openBoard(page);
  await drawRectangle(page);
  await expect.poll(() => boardElements(page)).toBe(1);
  await page.getByTestId("excalidraw-modal-close").click();
  await expect(page.getByTestId("excalidraw-modal")).toHaveCount(0);

  // Everything has reached the server: a browser of its own shows it all.
  const original = await inFreshBrowser(browser, page, `/doc/${ids.Handbook}`, (fresh) =>
    expectContentShown(fresh, shares),
  );
  const [originalBoard] = await boardsOf(page, ids.Handbook);
  expect(originalBoard).toBeDefined();

  // The dialog, with its subpages ticked.
  const { copy, sent } = await duplicateFromMenu(page, ids.Handbook, async (dialog) => {
    await expect(dialog.getByRole("heading")).toHaveText("Duplicate “Handbook”");
    await expect(dialog).toContainText("Every subpage is copied with all its content, drawings and files.");
    const box = dialog.getByRole("checkbox", { name: "Include its 2 subpages", exact: true });
    await expect(box).not.toBeChecked();
    await box.check();
    await dialog.getByTestId("duplicate-confirm").click();
  });
  expect(sent).toEqual({ include_subpages: true });
  await expect(page.getByTestId("doc-title")).toHaveValue("Handbook (copy)");
  await expect(rows(page)).toHaveText([
    "Handbook",
    "Chapter",
    "Section",
    "Handbook (copy)",
    "Chapter",
    "Section",
    "Appendix",
  ]);
  expect(await outline(page.request)).toEqual([
    "Handbook",
    "Handbook/Chapter",
    "Handbook/Chapter/Section",
    "Handbook (copy)",
    "Handbook (copy)/Chapter",
    "Handbook (copy)/Chapter/Section",
    "Appendix",
  ]);

  // The copy shows the same image, drawing and column widths, from a file
  // and a board of its own.
  const copied = await expectContentShown(page, shares);
  expect(copied.src).not.toBe(original.src);
  expect(copied.widths).toHaveLength(original.widths.length);
  copied.widths.forEach((w, i) => expect(Math.abs(w - original.widths[i]!)).toBeLessThanOrEqual(1));
  const [copyBoard] = await boardsOf(page, copy);
  expect(copyBoard).toBeDefined();
  expect(copyBoard!.id).not.toBe(originalBoard!.id);
  const svgOf = async (board: Board) => (await page.request.get(`/api/boards/${board.id}/svg`)).text();
  expect(await svgOf(copyBoard!)).toBe(await svgOf(originalBoard!));

  // Archive the original with its subpages: the copy still shows it all.
  page.once("dialog", (dialog) => {
    expect(dialog.message()).toContain('Move "Handbook" to the archive?');
    expect(dialog.message()).toContain("Its 2 subpages go with it.");
    void dialog.accept();
  });
  await page.getByTestId(`doc-row-${ids.Handbook}`).click({ button: "right" });
  await page.getByTestId("ctx-delete").click();
  await expect(page.getByTestId("toast-info")).toContainText("Moved to archive");
  await expect(rows(page)).toHaveText(["Handbook (copy)", "Chapter", "Section", "Appendix"]);
  await inFreshBrowser(browser, page, `/doc/${copy}`, (fresh) => expectContentShown(fresh, shares));

  // An owner can still open an archived page's files, so that check can't
  // tell whose they are. Delete the original for good: its image and board
  // are gone, and the copy still shows its own, with the drawing in the
  // board, not just in its preview.
  const purged = await page.request.post(`/api/docs/${ids.Handbook}/purge`, {
    headers: await writeHeaders(page.request),
  });
  expect(purged.status()).toBe(204);
  expect((await page.request.get(original.src)).status()).toBe(404);
  expect((await page.request.get(`/api/boards/${originalBoard!.id}/svg`)).status()).toBe(404);
  await inFreshBrowser(browser, page, `/doc/${copy}`, async (fresh) => {
    expect((await expectContentShown(fresh, shares)).src).toBe(copied.src);
    await openBoard(fresh);
    await expect.poll(() => boardElements(fresh)).toBe(1);
  });
});

