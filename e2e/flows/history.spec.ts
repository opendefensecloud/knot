import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "share_tokens","acl_invalidations","audit_events","doc_markdown_cache","doc_snapshots",
    "doc_updates","document_grants","documents","sessions","workspace_members","users",
    "workspaces","blobs","blob_bytes",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}
test.beforeAll(reset);

test("type → snapshot → edit → restore brings back the snapshot text", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  // V1: type the snapshot we'll restore to. With KNOT_SNAPSHOT_EVERY_N=1
  // the writer snapshots after every batch.
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("First version of the doc.");
  await page.waitForTimeout(500);

  // Open history; wait for the snapshot list to be non-empty.
  await page.getByTestId("open-history").click();
  await expect(page.getByTestId("history-drawer")).toBeVisible();
  const snapButtons = page.locator("[data-testid^='history-snap-']");
  await expect.poll(() => snapButtons.count(), { timeout: 10_000 }).toBeGreaterThan(0);
  // Close history; we'll come back after editing.
  await page.getByTestId("history-close").click();

  // V2: replace what's there with new text.
  await editor.click();
  await page.keyboard.press("Control+a");
  await page.keyboard.press("Delete");
  await page.keyboard.type("Completely different content.");
  await page.waitForTimeout(500);

  // Sanity: editor reflects V2 now.
  await expect(editor).toContainText("Completely different content.");

  // Re-open history; pick the OLDEST snapshot (lowest seq) which should be V1.
  await page.getByTestId("open-history").click();
  await expect(page.getByTestId("history-drawer")).toBeVisible();
  const lastSnap = snapButtons.last();
  await lastSnap.click();
  // Preview should contain "First version".
  await expect(page.getByTestId("history-preview")).toContainText("First version", {
    timeout: 5_000,
  });
  // Confirm prompt → accept; then click Restore.
  page.once("dialog", (d) => void d.accept());
  await page.getByTestId("history-restore").click();

  // Editor reflects V1 after the room update fans out.
  await expect(editor).toContainText("First version of the doc.", { timeout: 10_000 });
});
