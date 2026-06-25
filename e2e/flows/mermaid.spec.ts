import { execSync } from "node:child_process";

import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "comment_reactions", "comments",
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

test("mermaid: insert diagram, render svg, toggle source, edit", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@mermaid.test");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("hunter22!mermaid");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);

  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("Hello");

  // Click toolbar "Insert diagram"
  await page.getByTestId("toolbar-mermaid").click();

  // Mermaid node + rendered SVG should appear
  await expect(page.getByTestId("mermaid-node").first()).toBeVisible({ timeout: 8_000 });
  await expect(page.getByTestId("mermaid-svg").first()).toBeVisible({ timeout: 10_000 });
  await expect(page.locator("[data-testid='mermaid-svg'] svg").first()).toBeVisible();

  // Toggle to source view
  await page.getByTestId("mermaid-mode-source").first().click();
  // Source view replaces the SVG block
  await expect(page.getByTestId("mermaid-svg")).toHaveCount(0);

  // Toggle back
  await page.getByTestId("mermaid-mode-preview").first().click();
  await expect(page.getByTestId("mermaid-svg").first()).toBeVisible({ timeout: 8_000 });
});
