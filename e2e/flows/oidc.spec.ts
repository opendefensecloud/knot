import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);
// The second test signs in as the owner the first one sets up.
test.describe.configure({ mode: "serial" });

test("Dex round-trip lands a session", async ({ page }) => {
  // Before any OIDC login can succeed, a workspace must exist for
  // auto-provision to land in. Create it via /auth/setup with a throwaway
  // local user — OIDC then provisions the Dex user into that workspace.
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@example.com");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/$/);

  // Sign out, then sign in via OIDC.
  await page.goto("/settings");
  await page.getByTestId("logout").click();
  await page.waitForURL(/\/login/);

  await page.click("text=Continue with SSO");

  // Dex login page (URL contains :5556).
  await page.waitForURL(/:5556\/dex/);
  // Dex password connector uses name="login" for email, name="password" for password.
  await page.locator('input[name="login"]').fill("alice@example.com");
  await page.locator('input[name="password"]').fill("password");
  await page.locator('button[type="submit"]').click();

  // skipApprovalScreen is true in the Dex config — should redirect straight back.
  // If a Grant Access prompt appears anyway, click it.
  const grant = page.locator("text=Grant Access");
  if (await grant.isVisible({ timeout: 1000 }).catch(() => false)) {
    await grant.click();
  }

  // Back at knot, authenticated. Auto-provision rule "always" attaches
  // alice@example.com to the existing workspace.
  await page.waitForURL("http://localhost:5173/", { timeout: 15_000 });
  await expect(page.getByTestId("sidebar")).toBeVisible({ timeout: 5_000 });
});

// Relies on the workspace owner the test above creates.
test("a signed-out colleague following a page link lands on that page after SSO", async ({ browser }) => {
  // The owner (password login) creates a page and copies its link.
  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();
  await owner.goto("/login");
  await owner.getByTestId("login-email").fill("owner@example.com");
  await owner.getByTestId("login-password").fill("owner-hunter22");
  await owner.getByTestId("login-submit").click();
  await owner.waitForURL(/\/(?:doc\/.+)?$/);
  // Two pages, and the link goes to the one the start page does NOT open:
  // a signed-in visit to "/" jumps to the first page in the tree, so linking
  // that one would pass even when the link target is lost.
  for (let i = 0; i < 2; i++) {
    const before = owner.url();
    await owner.getByTestId("new-doc").click();
    await owner.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
    await owner.getByTestId("new-doc-blank").click();
    await owner.waitForURL((u) => /\/doc\/.+/.test(u.pathname) && u.href !== before);
  }
  const ids = await owner.evaluate(async () => {
    const r = await fetch("/api/docs", { credentials: "include" });
    return ((await r.json()) as { id: string }[]).map((d) => d.id);
  });
  expect(ids).toHaveLength(2);
  const docPath = `/doc/${ids[1]}`;
  await ownerCtx.close();

  // A colleague without a session opens the link and signs in with SSO.
  const colleagueCtx = await browser.newContext();
  const colleague = await colleagueCtx.newPage();
  await colleague.goto(docPath);
  await colleague.waitForURL(/\/login/);
  await colleague.click("text=Continue with SSO");
  await colleague.waitForURL(/:5556\/dex/);
  await colleague.locator('input[name="login"]').fill("alice@example.com");
  await colleague.locator('input[name="password"]').fill("password");
  await colleague.locator('button[type="submit"]').click();
  const grant = colleague.locator("text=Grant Access");
  if (await grant.isVisible({ timeout: 1000 }).catch(() => false)) {
    await grant.click();
  }

  // Back at knot on the linked page, not the start page.
  await colleague.waitForURL(`http://localhost:5173${docPath}`, { timeout: 15_000 });
  await expect(colleague.getByTestId("doc-title")).toBeVisible({ timeout: 5_000 });
  await colleagueCtx.close();
});
