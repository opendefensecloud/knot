/**
 * e2e: the table of contents box. Inserted from the toolbar it lists the
 * page's headings, its depth is set with Levels, a click lands the heading
 * below the sticky band and puts its id in the URL, the Markdown export
 * carries `[[_TOC_]]` with the depth comment, and a public share link shows
 * the box with working anchors.
 */
import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

import { setupOwner } from "./tree-helpers";

test.beforeEach(reset);

test("table of contents: insert, depth, jump, export, public page", async ({ page, browser }) => {
  test.setTimeout(90_000);
  await setupOwner(page);

  await page.getByTestId("new-doc").click();
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  const docUrl = page.url();
  const docId = docUrl.match(/\/doc\/([^/?#]+)/)![1]!;
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.getByTestId("toolbar-toc").click();
  // Below the box: headings typed with Markdown shortcuts, spaced out so
  // the page scrolls.
  await page.keyboard.press("ArrowDown");
  const filler = "Lorem ipsum dolor sit amet. ".repeat(30);
  for (const [prefix, title] of [
    ["# ", "Plan"],
    ["## ", "Tasks"],
    ["## ", "Tasks"],
    ["### ", "Details"],
  ] as const) {
    await page.keyboard.type(`${prefix}${title}`);
    await page.keyboard.press("Enter");
    await page.keyboard.type(filler);
    await page.keyboard.press("Enter");
  }

  const box = page.getByTestId("toc-box");
  const entries = box.getByTestId("toc-entry");
  await expect(entries).toHaveText(["Plan", "Tasks", "Tasks", "Details"]);

  await box.getByTestId("toc-levels").selectOption("2");
  await expect(entries).toHaveText(["Plan", "Tasks", "Tasks"]);

  // The second "Tasks" is #h-tasks-1; it lands below the sticky band.
  await entries.nth(2).click();
  await expect(page).toHaveURL(/#h-tasks-1$/);
  const heading = page.locator("h2#h-tasks-1");
  await expect(heading).toBeVisible();
  await expect
    .poll(async () => {
      const top = await heading.evaluate((h) => h.getBoundingClientRect().top);
      const band = await page.evaluate(() =>
        Math.max(
          0,
          ...Array.from(document.querySelectorAll("[data-sticky-band]")).map(
            (b) => b.getBoundingClientRect().bottom,
          ),
        ),
      );
      return top >= band && top < band + 80;
    })
    .toBe(true);

  // The Markdown export carries the marker with the depth comment.
  await expect
    .poll(async () => (await page.request.get(`/api/docs/${docId}/markdown`)).text(), { timeout: 10_000 })
    .toContain("<!-- knot:toc maxLevel=2 -->\n[[_TOC_]]");

  // Opening the page at the anchor scrolls there.
  await page.goto(`${docUrl}#h-details`);
  await expect(page.locator("h3#h-details")).toBeInViewport({ timeout: 10_000 });

  // Public share link: the box with anchors, and ids on the headings.
  await page.goto(`${docUrl}/permissions`);
  await page.getByTestId("share-enable").click();
  const shareUrl = await page.getByTestId("share-url").inputValue();
  const anonCtx = await browser.newContext();
  const anon = await anonCtx.newPage();
  await anon.goto(shareUrl.replace(/^https?:\/\/[^/]+/, ""));
  const doc = anon.frameLocator("iframe[title='Public document']");
  await expect(doc.locator(".knot-box a[href='#h-tasks-1']")).toBeVisible({ timeout: 8_000 });
  await expect(doc.locator("h2#h-tasks-1")).toHaveText("Tasks");
  await expect(doc.locator("nav.knot-box[aria-label='Contents']")).not.toContainText("Details");
  await anonCtx.close();
});
