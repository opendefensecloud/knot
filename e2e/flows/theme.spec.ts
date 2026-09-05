import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);

test("theme toggle persists across reload", async ({ page }) => {
  // Setup owner so the workspace sidebar (which hosts the toggle) renders.
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@theme.test");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("hunter22!theme");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");

  // The theme toggle now lives on the Settings page (moved out of the
  // sidebar in 0988b51). data-theme is applied to <html> globally, so the
  // assertions below hold regardless of which page we're on.
  await page.goto("/settings");

  const toggle = page.getByTestId("theme-toggle");
  await expect(toggle).toBeVisible();
  await toggle.click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");

  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");

  await page.getByTestId("theme-toggle").click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
});
