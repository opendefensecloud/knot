import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);

test("skin choice persists across reload", async ({ page }) => {
  // Setup owner so the workspace renders.
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@skin.test");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("hunter22!skin");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  const html = page.locator("html");
  await expect(html).toHaveAttribute("data-skin", "light");
  await expect(html).toHaveAttribute("data-theme", "light");

  // The picker lives on the Settings page. Both attributes are stamped on
  // <html> globally, so the assertions hold regardless of the page.
  await page.goto("/settings");

  const nord = page.getByTestId("skin-nord");
  await expect(nord).toBeVisible();
  await nord.click();
  await expect(nord).toHaveAttribute("aria-pressed", "true");
  await expect(html).toHaveAttribute("data-skin", "nord");
  await expect(html).toHaveAttribute("data-theme", "dark");

  await page.reload();
  await expect(html).toHaveAttribute("data-skin", "nord");
  await expect(html).toHaveAttribute("data-theme", "dark");

  await page.getByTestId("skin-paper").click();
  await expect(html).toHaveAttribute("data-skin", "paper");
  await expect(html).toHaveAttribute("data-theme", "light");
});

test("a pre-skin dark preference lands on the Dark skin", async ({ page }) => {
  await page.goto("/login");
  await page.evaluate(() => {
    localStorage.removeItem("knot.skin");
    localStorage.setItem("knot.theme", "dark");
  });
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-skin", "dark");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
});
