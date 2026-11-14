/**
 * e2e: starring pages from the sidebar tree, opening them from the
 * Favorites section, the order surviving a reload, and unstarring.
 *
 * Pages are seeded through the API (tree-helpers). Favorites are read back
 * from GET /api/me/favorites as well as from the sidebar, so the spec checks
 * what the server stored, not only what the optimistic cache shows.
 */
import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

import { reset } from "../support/reset";

import { seedPages, setupOwner, showTree } from "./tree-helpers";

test.beforeEach(reset);

/** The favorites' page ids, in the order the server returns them. */
async function storedFavorites(request: APIRequestContext): Promise<string[]> {
  const res = await request.get("/api/me/favorites");
  expect(res.status()).toBe(200);
  return ((await res.json()) as { doc_id: string }[]).map((f) => f.doc_id);
}

/** The Favorites section's page ids, top to bottom. */
async function shownFavorites(page: Page): Promise<string[]> {
  const ids = await page
    .getByTestId("favorites")
    .locator("[data-testid^='fav-link-']")
    .evaluateAll((els) => els.map((el) => el.getAttribute("data-testid")!.slice("fav-link-".length)));
  return ids;
}

/** Stars a page from its tree row: the star shows on hover. */
async function starFromTree(page: Page, id: string): Promise<void> {
  await page.getByTestId(`tree-row-${id}`).hover();
  const star = page.getByTestId(`tree-star-${id}`);
  await expect(star).toBeEnabled();
  await expect(star).toHaveAccessibleName("Add to favorites");
  await star.click();
  await expect(star).toHaveAccessibleName("Remove from favorites");
}

test("star from the tree, open from Favorites, reload, unstar", async ({ page }) => {
  await setupOwner(page);
  const { Alpha, Beta } = await seedPages(page.request, [{ title: "Alpha" }, { title: "Beta" }]);
  await showTree(page, [Alpha, Beta]);
  await expect(page.getByTestId("favorites")).toHaveCount(0);

  // Starring a row puts it in the Favorites section, and opening it from
  // there goes to the page.
  await starFromTree(page, Beta);
  await expect(page.getByTestId(`fav-link-${Beta}`)).toHaveText("Beta");
  await page.getByTestId(`fav-link-${Beta}`).click();
  await page.waitForURL(new RegExp(`/doc/${Beta}$`));

  // Oldest star first.
  await starFromTree(page, Alpha);
  await expect.poll(() => shownFavorites(page)).toEqual([Beta, Alpha]);
  await expect.poll(() => storedFavorites(page.request)).toEqual([Beta, Alpha]);

  await page.reload();
  await expect(page.getByTestId(`fav-link-${Beta}`)).toBeVisible({ timeout: 10_000 });
  expect(await shownFavorites(page)).toEqual([Beta, Alpha]);

  // Unstarring from the section removes it and stays on the page.
  await page.getByTestId(`fav-row-${Beta}`).hover();
  await page.getByTestId(`fav-unstar-${Beta}`).click();
  await expect(page.getByTestId(`fav-row-${Beta}`)).toHaveCount(0);
  await expect(page).toHaveURL(new RegExp(`/doc/${Beta}$`));
  await expect.poll(() => storedFavorites(page.request)).toEqual([Alpha]);

  // The row menu knows Alpha is still a favorite.
  await page.getByTestId(`doc-row-${Alpha}`).click({ button: "right" });
  await expect(page.getByTestId("ctx-favorite")).toHaveText("Remove from favorites");
});
