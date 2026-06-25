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

test("editor connects, accepts typing, persists across reload", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("e@example.com");
  await page.getByTestId("setup-display-name").fill("E");
  await page.getByTestId("setup-password").fill("hunter22!hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  const url = page.url();

  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  await page.locator("[data-testid='editor-host'] .ProseMirror").click();
  await page.keyboard.type("Editor smoke test.");
  await page.waitForTimeout(800);
  await page.goto(url);
  await expect(page.locator("[data-testid='editor-host']")).toContainText("Editor smoke test.", {
    timeout: 10_000,
  });
});
