// Document archive (#37): deleting a folder moves it and its subpages to the
// archive, where it can be previewed read-only and restored; owners can
// delete it permanently.
import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeEach(reset);

async function setupOwner(page: Page) {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@example.com");
  await page.getByTestId("setup-display-name").fill("Olive");
  await page.getByTestId("setup-password").fill("hunter22!hunter22");
  await page.getByTestId("setup-submit").click();
  await expect(page.getByTestId("doc-tree")).toBeVisible({ timeout: 10_000 });
}

async function writeHeaders(page: Page) {
  const csrf = (await page.context().cookies()).find((c) => c.name === "csrf");
  if (!csrf) throw new Error("csrf cookie not found");
  return { "X-CSRF-Token": csrf.value };
}

async function createDoc(page: Page, title: string, parentId?: string): Promise<string> {
  const res = await page.request.post("/api/docs", {
    headers: await writeHeaders(page),
    data: parentId ? { title, parent_id: parentId } : { title },
  });
  expect(res.status()).toBe(201);
  return (await res.json()).id as string;
}

async function searchHits(page: Page, q: string): Promise<string[]> {
  const res = await page.request.get(`/api/search?q=${encodeURIComponent(q)}`);
  expect(res.status()).toBe(200);
  return ((await res.json()) as { results: { doc_id: string }[] }).results.map((h) => h.doc_id);
}

test("archive a folder, preview it, restore it, then delete it permanently", async ({ page }) => {
  await setupOwner(page);
  const folder = await createDoc(page, "Quarterly Projects");
  const first = await createDoc(page, "Kickoff Notes", folder);
  const second = await createDoc(page, "Retro Notes", folder);
  await page.reload();
  for (const id of [folder, first, second]) {
    await expect(page.getByTestId(`doc-row-${id}`)).toBeVisible();
  }

  expect(await searchHits(page, "Kickoff")).toContain(first);

  // Move the folder to the archive from the sidebar menu.
  page.once("dialog", (dialog) => {
    expect(dialog.message()).toContain('Move "Quarterly Projects" to the archive?');
    expect(dialog.message()).toContain("Its 2 subpages go with it.");
    void dialog.accept();
  });
  await page.getByTestId(`doc-row-${folder}`).click({ button: "right" });
  await page.getByTestId("ctx-delete").click();
  await expect(page.getByTestId("toast-info")).toContainText("Moved to archive");
  for (const id of [folder, first, second]) {
    await expect(page.getByTestId(`doc-row-${id}`)).toHaveCount(0);
  }
  expect(await searchHits(page, "Kickoff")).not.toContain(first);

  // The archive lists the folder; its preview is read-only with a banner.
  await page.getByTestId("sidebar-archive").click();
  const row = page.getByTestId(`archive-row-${folder}`);
  await expect(row).toContainText("+2 subpages");
  await expect(row).toContainText("Deleted by Olive");
  await row.getByRole("link", { name: "Open" }).click();
  await page.waitForURL(new RegExp(`/doc/${folder}$`));
  const banner = page.getByTestId("archived-banner");
  await expect(banner).toContainText("In the archive");
  await expect(page.getByTestId("doc-title")).toHaveAttribute("readonly", "");
  await expect(page.getByTestId("toggle-edit-mode")).toHaveCount(0);

  // Restore from the banner: the whole tree comes back.
  await banner.getByRole("button", { name: "Restore" }).click();
  await expect(page.getByTestId("archived-banner")).toHaveCount(0);
  // Restoring revokes the preview's socket (4403); the page must stay and
  // reconnect, not bounce to / with "You no longer have access".
  await page.waitForTimeout(1_000);
  await expect(page).toHaveURL(new RegExp(`/doc/${folder}$`));
  await expect(page.getByTestId("toast-error")).toHaveCount(0);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
  for (const id of [folder, first, second]) {
    await expect(page.getByTestId(`doc-row-${id}`)).toBeVisible();
  }
  await expect(page.getByTestId("toggle-edit-mode")).toBeVisible();

  // Archive again and delete permanently from the archive page.
  page.once("dialog", (dialog) => void dialog.accept());
  await page.getByTestId(`doc-row-${folder}`).click({ button: "right" });
  await page.getByTestId("ctx-delete").click();
  await expect(page.getByTestId(`doc-row-${folder}`)).toHaveCount(0);
  await page.getByTestId("sidebar-archive").click();
  page.once("dialog", (dialog) => {
    expect(dialog.message()).toContain('Permanently delete "Quarterly Projects" and its 2 subpages?');
    void dialog.accept();
  });
  await page
    .getByTestId(`archive-row-${folder}`)
    .getByRole("button", { name: "Delete permanently" })
    .click();
  await expect(page.getByText("Nothing in the archive.")).toBeVisible();
  for (const id of [folder, first, second]) {
    const res = await page.request.get(`/api/docs/${id}`);
    expect([403, 404]).toContain(res.status());
  }
});
