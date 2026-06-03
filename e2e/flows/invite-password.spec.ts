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

test("owner invites Bob with password; Bob signs in", async ({ browser }) => {
  // Owner sets up + invites Bob with password.
  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();
  await owner.goto("/setup");
  await owner.getByTestId("setup-email").fill("owner@example.com");
  await owner.getByTestId("setup-display-name").fill("Owner");
  await owner.getByTestId("setup-password").fill("owner-hunter22");
  await owner.getByTestId("setup-submit").click();
  await owner.waitForURL(/\/(?:doc\/.+)?$/);

  await owner.goto("/members");
  await owner.getByTestId("invite-email").fill("bob@example.com");
  await owner.getByTestId("invite-role").selectOption("editor");
  await owner.getByTestId("invite-password").fill("bob-hunter22");
  await owner.getByTestId("invite-submit").click();
  await expect(owner.locator("[data-testid^='member-']")).toHaveCount(2, { timeout: 5_000 });

  // Bob signs in in a fresh context.
  const bobCtx = await browser.newContext();
  const bob = await bobCtx.newPage();
  await bob.goto("/login");
  await bob.getByTestId("login-email").fill("bob@example.com");
  await bob.getByTestId("login-password").fill("bob-hunter22");
  await bob.getByTestId("login-submit").click();
  await bob.waitForURL(/\/(?:doc\/.+)?$/);
  await expect(bob.getByTestId("sidebar")).toBeVisible();

  await ownerCtx.close();
  await bobCtx.close();
});

test("invite without password for unknown user returns user-not-found", async ({ page }) => {
  reset();
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner2@example.com");
  await page.getByTestId("setup-display-name").fill("Owner2");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await page.goto("/members");
  await page.getByTestId("invite-email").fill("unknown@example.com");
  await page.getByTestId("invite-role").selectOption("editor");
  // leave invite-password empty
  await page.getByTestId("invite-submit").click();
  await expect(page.getByTestId("toast-error")).toBeVisible({ timeout: 3_000 });
});
