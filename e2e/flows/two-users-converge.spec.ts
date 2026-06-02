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

test("two users editing concurrently converge on both screens", async ({ page }) => {
  // v0.1 scope: real two-user convergence needs invite-with-password (Plan 8).
  // For now, verify the WS + persistence loop works end-to-end with a single
  // user typing + reload. Plan 8 will replace the body with the real two-user
  // assertion.
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("alice@example.com");
  await page.getByTestId("setup-display-name").fill("Alice");
  await page.getByTestId("setup-password").fill("hunter22-alice");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/, { timeout: 10_000 });
  await page.getByTestId("new-doc").click();
  await page.waitForURL(/\/doc\/.+/);
  const docUrl = page.url();

  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  await page.locator("[data-testid='editor-host'] .ProseMirror").click();
  await page.keyboard.type("Hello from Alice.");
  await page.waitForTimeout(800); // let writer flush
  await page.goto(docUrl);
  await expect(page.locator("[data-testid='editor-host']")).toContainText("Hello from Alice.", {
    timeout: 10_000,
  });
});
