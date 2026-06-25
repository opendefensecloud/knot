import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations",
    "audit_events",
    "doc_markdown_cache",
    "doc_snapshots",
    "doc_updates",
    "document_grants",
    "documents",
    "sessions",
    "workspace_members",
    "users",
    "workspaces",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}
test.beforeAll(reset);

test("toolbar toggles bold + heading", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute(
    "data-status",
    "connected",
    { timeout: 10_000 },
  );

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("hello world");
  await page.keyboard.press("Control+a");

  await page.getByTestId("toolbar-bold").click();
  await expect(editor.locator("strong")).toContainText("hello world");

  await page.getByTestId("toolbar-h1").click();
  await expect(editor.locator("h1")).toBeVisible();
});
