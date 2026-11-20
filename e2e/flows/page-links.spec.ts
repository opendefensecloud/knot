/**
 * e2e: `[[` in the editor. Picking a page inserts a link to it; the create
 * row adds the typed title as a subpage of the current page, links it, and
 * leaves the user on the current page. Results are read back from the API
 * where the server holds them.
 */
import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

import { seedPages, setupOwner } from "./tree-helpers";

test.beforeEach(reset);

type ApiDoc = { id: string; parent_id: string | null; title: string };

test("[[ links an existing page and creates a missing one as a subpage", async ({ page }) => {
  await setupOwner(page);
  const { Urlaub } = await seedPages(page.request, [
    { title: "Team" },
    { title: "Urlaub", parent: "Team" },
  ]);

  // A new blank page opens in edit mode.
  await page.getByTestId("new-doc").click();
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  const current = page.url().match(/\/doc\/([^/?#]+)/)?.[1] ?? "";
  expect(current).toBeTruthy();
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();

  // Existing page: filter, then Enter.
  await page.keyboard.type("Siehe [[urla");
  const popup = page.locator(".knot-page-link-popup");
  await expect(popup.getByTestId("page-link-item").first()).toContainText("Urlaub");
  await expect(popup.getByTestId("page-link-item").first()).toContainText("Team");
  await page.keyboard.press("Enter");
  await expect(editor.locator(`a[href="knot://doc/${Urlaub}"]`)).toHaveText("Urlaub");
  await expect(editor).not.toContainText("[[");

  // Missing page: the create row.
  await page.keyboard.type("und [[Reiseplan");
  await popup.getByTestId("page-link-create").click();
  await expect.poll(async () => {
    const res = await page.request.get("/api/docs");
    const docs = (await res.json()) as ApiDoc[];
    return docs.find((d) => d.title === "Reiseplan")?.parent_id ?? null;
  }).toBe(current);
  const docs = (await (await page.request.get("/api/docs")).json()) as ApiDoc[];
  const created = docs.find((d) => d.title === "Reiseplan")!;
  await expect(editor.locator(`a[href="knot://doc/${created.id}"]`)).toHaveText("Reiseplan");
  expect(new URL(page.url()).pathname).toBe(`/doc/${current}`);
});
