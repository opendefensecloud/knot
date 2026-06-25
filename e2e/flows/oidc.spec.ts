import { execSync } from "node:child_process";

import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations", "audit_events", "doc_markdown_cache",
    "doc_snapshots", "doc_updates", "document_grants", "documents",
    "sessions", "workspace_members", "users", "workspaces",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}

test.beforeAll(reset);

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
