import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);

test("toolbar toggles bold + heading", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute(
    "data-status",
    "connected",
    { timeout: 10_000 },
  );

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("hello world");
  await page.keyboard.press("ControlOrMeta+a");

  await page.getByTestId("toolbar-bold").click();
  await expect(editor.locator("strong")).toContainText("hello world");

  await page.getByTestId("toolbar-h1").click();
  await expect(editor.locator("h1")).toBeVisible();
});
