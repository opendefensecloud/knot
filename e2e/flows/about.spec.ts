import { expect, request, test } from "@playwright/test";

import { reset } from "../support/reset";

const EMAIL = "owner@about.test";
const PASSWORD = "hunter22!about";

test.beforeAll(async () => {
  reset();
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD, display_name: "Owner" }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});

test("the sidebar names the running version and opens its release notes", async ({ page }) => {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  // "knot dev" from `cargo run`, "knot 0.9.0" from a release build.
  const link = page.getByTestId("sidebar-version");
  await expect(link).toHaveText(/^knot \S+$/);
  const version = (await link.textContent())!.replace(/^knot /, "");

  // A brand-new user starts caught up, whatever the build: no upgrade card.
  await expect(page.getByTestId("whats-new-card")).toHaveCount(0);

  await link.click();
  await page.waitForURL(/\/about$/);
  await expect(page.getByTestId("about-version")).toHaveText(version);
  await expect(page.getByTestId("about-commit")).toHaveText(/^\S{1,7}$/);

  // Every release ever made stays in CHANGELOG.md, so this one always shows.
  const release = page.getByTestId("release-0.8.1");
  const summary = release.locator("summary");
  await expect(summary).toContainText("0.8.1");
  // Open while it is the newest release, collapsed once a later one ships.
  if ((await release.getAttribute("open")) === null) await summary.click();
  await expect(release.getByRole("heading", { name: "Security" })).toBeVisible();
});

test("what's new is only for signed-in users", async () => {
  const ctx = await request.newContext({ baseURL: "http://localhost:3000" });
  expect((await ctx.get("/api/whats-new")).status()).toBe(401);
  expect((await ctx.get("/api/release-notes")).status()).toBe(401);
});
