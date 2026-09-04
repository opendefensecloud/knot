// Templates: marking a doc as a template, and creating a new doc from one.
//
// Both paths run through handlers that were inline `async` props until the
// eslint cleanup extracted them (DocPage's toggle-template, DocTree's
// onPickTemplate). Neither had e2e coverage, which is why this exists.
import { execSync } from "node:child_process";

import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations", "audit_events", "doc_markdown_cache", "doc_tasks",
    "doc_snapshots", "doc_updates", "document_grants", "documents",
    "sessions", "workspace_members", "users", "workspaces",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}

test.beforeEach(reset);

test("save as template, then create a doc from it", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("t@example.com");
  await page.getByTestId("setup-display-name").fill("T");
  await page.getByTestId("setup-password").fill("hunter22!hunter22");
  await page.getByTestId("setup-submit").click();

  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  await page.getByTestId("doc-title").fill("Standup Notes");
  await page.locator("[data-testid='editor-host'] .ProseMirror").click();
  await page.keyboard.type("Agenda for the day.");
  await page.waitForTimeout(900);

  // DocPage handler under test.
  await page.getByTestId("toggle-template").click();
  await expect(page.getByTestId("toast-info")).toContainText("Saved as template", {
    timeout: 10_000,
  });
  await expect(page.getByTestId("toggle-template")).toHaveAttribute("aria-pressed", "true");

  // DocTree handler under test: the picker must list it and creating from it
  // must navigate to a new doc carrying the template's body.
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  const card = page.locator("[data-testid^='template-card-']").first();
  await expect(card).toContainText("Standup Notes", { timeout: 10_000 });
  const templateUrl = page.url();
  await card.click();

  await page.waitForURL((u) => /\/doc\/.+/.test(u.href) && u.href !== templateUrl, {
    timeout: 10_000,
  });
  await expect(page.locator("[data-testid='editor-host']")).toContainText("Agenda for the day.", {
    timeout: 10_000,
  });
  await expect(page.getByTestId("doc-title")).toHaveValue("Standup Notes");
});
