import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);

test("follow system switches with the OS appearance", async ({ page }) => {
  await page.emulateMedia({ colorScheme: "light" });

  // Setup owner so the Settings page renders.
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@skin-auto.test");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("hunter22!skin-auto");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await page.goto("/settings");
  await page.locator("label", { hasText: "Follow system" }).click();
  // In an automatic mode each card fills its own scheme's slot.
  await page.getByTestId("skin-paper").click();
  await page.getByTestId("skin-nord").click();

  const html = page.locator("html");
  await expect(html).toHaveAttribute("data-skin", "paper");
  await expect(page.getByTestId("skin-paper").getByTestId("skin-now")).toBeVisible();

  // The OS going dark switches the skin live, no reload.
  await page.emulateMedia({ colorScheme: "dark" });
  await expect(html).toHaveAttribute("data-skin", "nord");
  await expect(html).toHaveAttribute("data-theme", "dark");

  await page.reload();
  await expect(html).toHaveAttribute("data-skin", "nord");
  await expect(html).toHaveAttribute("data-theme", "dark");

  await page.emulateMedia({ colorScheme: "light" });
  await expect(html).toHaveAttribute("data-skin", "paper");
});

/** Stores sun mode with Paper by day and Nord by night, plus any extra
 *  preferences, the way the Settings page would. */
async function storeSunPrefs(page: Page, extra: Record<string, string> = {}) {
  const prefs = { "knot.skinMode": "sun", "knot.skinLight": "paper", "knot.skinDark": "nord", ...extra };
  await page.evaluate((entries) => {
    for (const [key, value] of Object.entries(entries)) localStorage.setItem(key, value);
  }, prefs);
}

test.describe("follow the sun", () => {
  // Without a typed location the sun is placed by the time zone, so pin it.
  test.use({ timezoneId: "Europe/Berlin" });

  test("light by day, dark after sunset, dark from first paint", async ({ page }) => {
    // 21:00 in Berlin on the longest day, half an hour before its sunset at
    // about 19:33 UTC. Only the time zone's table entry puts the sun up then:
    // the fallback for a zone it can't place would already be in the night.
    await page.clock.install({ time: new Date("2026-06-21T19:00:00Z") });
    await page.goto("/login");
    await storeSunPrefs(page);
    await page.reload();

    const html = page.locator("html");
    await expect(html).toHaveAttribute("data-skin", "paper");
    await expect(html).toHaveAttribute("data-theme", "light");

    // Past sunset, the minute re-check switches the skin without a reload.
    await page.clock.setSystemTime(new Date("2026-06-21T19:40:00Z"));
    await page.clock.runFor(61_000);
    await expect(html).toHaveAttribute("data-skin", "nord");

    await page.reload();
    await expect(html).toHaveAttribute("data-skin", "nord");
    await expect(html).toHaveAttribute("data-theme", "dark");
  });

  test("a typed location wins over the time zone", async ({ page }) => {
    // Midday in Berlin, but Tromsø is in polar night.
    await page.clock.install({ time: new Date("2026-12-21T11:00:00Z") });
    await page.goto("/login");
    await storeSunPrefs(page, { "knot.sunLocation": '{"lat":69.65,"lon":18.96}' });
    await page.reload();

    const html = page.locator("html");
    await expect(html).toHaveAttribute("data-skin", "nord");
    await expect(html).toHaveAttribute("data-theme", "dark");
  });
});
