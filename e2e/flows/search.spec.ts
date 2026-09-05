import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);

test("Cmd+K search finds a doc by title and navigates to it", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  for (const t of ["Findable Alpha", "Other Beta", "Some Gamma"]) {
    await page.getByTestId("new-doc").click();
    await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
    await page.getByTestId("new-doc-blank").click();
    await page.waitForURL(/\/doc\/.+/);
    const input = page.locator("[data-testid='doc-title']");
    await expect(input).toHaveValue("Untitled");
    const patch = page.waitForResponse(
      (r) => r.url().includes("/api/docs/") && r.request().method() === "PATCH",
    );
    await input.fill(t);
    await input.blur();
    await patch;
  }

  await page.keyboard.press("Control+k");
  await expect(page.getByTestId("cmdk")).toBeVisible();
  await page.getByTestId("cmdk-input").fill("findable");

  const hit = page.locator("[data-testid^='cmdk-item-doc:']").first();
  await expect(hit).toContainText("Findable", { timeout: 5_000 });

  await page.keyboard.press("Enter");
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("doc-title")).toHaveValue("Findable Alpha");
});
