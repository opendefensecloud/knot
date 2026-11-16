/**
 * e2e: callout boxes. A box is made from the toolbar, gets a title and a
 * body, changes its kind with the select, comes out of the Markdown export
 * as a GitLab alert with a title, and shows as a box on a public share link.
 */
import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

import { setupOwner } from "./tree-helpers";

test.beforeEach(reset);

test("callouts: insert, title, kind, export, public page", async ({ page, browser }) => {
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
  await page.getByTestId("toolbar-blockquote").click();
  await page.getByTestId("toolbar-callout-warning").click();

  const box = editor.getByTestId("callout");
  await expect(box).toHaveAttribute("data-kind", "warning");
  // The caret is in the box's body.
  await page.keyboard.type("Am Freitag ist der Dienst kurz weg.");
  await expect(box.locator(".knot-callout__body")).toHaveText("Am Freitag ist der Dienst kurz weg.");

  // The title field shows a visible focus ring when reached by keyboard.
  await box.getByTestId("callout-kind").focus();
  await page.keyboard.press("Shift+Tab");
  await expect(box.getByTestId("callout-title")).toBeFocused();
  const ring = await box
    .getByTestId("callout-title")
    .evaluate((el) => ({ style: getComputedStyle(el).outlineStyle, width: getComputedStyle(el).outlineWidth }));
  expect(ring.style).not.toBe("none");
  expect(parseFloat(ring.width)).toBeGreaterThanOrEqual(2);

  await box.getByTestId("callout-title").fill("Wartung");
  // Each kind has its own colour, in the light skins too.
  const headColour = () => box.locator(".knot-callout__head").evaluate((h) => getComputedStyle(h).backgroundColor);
  const warningColour = await headColour();
  await box.getByTestId("callout-kind").selectOption("caution");
  await expect(box).toHaveAttribute("data-kind", "caution");
  await expect.poll(headColour).not.toBe(warningColour);

  await expect
    .poll(async () => (await page.request.get(`/api/docs/${docId}/markdown`)).text(), { timeout: 10_000 })
    .toContain("> [!caution] Wartung\n> Am Freitag ist der Dienst kurz weg.");

  await page.goto(`${docUrl}/permissions`);
  await page.getByTestId("share-enable").click();
  const shareUrl = await page.getByTestId("share-url").inputValue();
  const anonCtx = await browser.newContext();
  const anon = await anonCtx.newPage();
  await anon.goto(shareUrl.replace(/^https?:\/\/[^/]+/, ""));
  const doc = anon.frameLocator("iframe[title='Public document']");
  const shared = doc.locator(".knot-callout.knot-callout--caution");
  await expect(shared.locator(".knot-callout__title")).toHaveText("Wartung", { timeout: 8_000 });
  await expect(shared).toContainText("Am Freitag ist der Dienst kurz weg.");
  await anonCtx.close();
});
