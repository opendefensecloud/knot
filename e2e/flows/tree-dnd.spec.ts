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

test("dragging Child onto Parent doesn't crash; no error toast", async ({
  page,
}) => {
  // v0.1 dnd-kit test: assert the drag flow doesn't crash and no error toast
  // appears. Asserting the resulting hierarchy via UI is brittle because the
  // tree component doesn't visually nest until query invalidates and refetches.
  // The server-side `move` integration tests (Plan 4) already guarantee the
  // POST /api/docs/:id/move semantics.
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();

  // The "new-doc" button now opens a NewDocPicker modal; click through to
  // "new-doc-blank" to create a blank document.
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  const firstDocUrl = page.url();
  await page.locator("[data-testid='doc-title']").fill("Parent");
  await page.locator("[data-testid='doc-title']").blur();

  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  // Wait for URL to change to the SECOND doc's URL (not the first).
  await page.waitForFunction(
    (prev: string) => window.location.href !== prev,
    firstDocUrl,
    { timeout: 10_000 },
  );
  // Must not still be on the first doc URL.
  await expect(page).not.toHaveURL(firstDocUrl);
  await page.locator("[data-testid='doc-title']").fill("Child");
  await page.locator("[data-testid='doc-title']").blur();

  const rows = page.locator("[data-testid^='doc-row-']");
  await expect(rows).toHaveCount(2, { timeout: 5_000 });

  // Use the first and second rows by index rather than text content.
  // The rename PATCH is async and the tree may briefly show "Untitled"
  // while the mutation is in-flight; matching by text is fragile here.
  const row0 = rows.nth(0);
  const row1 = rows.nth(1);
  await expect(row0).toBeVisible({ timeout: 5_000 });
  await expect(row1).toBeVisible({ timeout: 5_000 });

  const childBox = await row1.boundingBox();
  const parentBox = await row0.boundingBox();
  if (!childBox || !parentBox) throw new Error("rows not laid out");

  // Manual drag — dnd-kit PointerSensor needs movement past the activation
  // threshold (distance: 6) to start dragging.
  await page.mouse.move(childBox.x + 10, childBox.y + childBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(childBox.x + 20, childBox.y + childBox.height / 2, {
    steps: 5,
  });
  await page.mouse.move(
    parentBox.x + parentBox.width / 2,
    parentBox.y + parentBox.height / 2,
    { steps: 10 },
  );
  await page.mouse.up();

  await page.waitForTimeout(500);

  await expect(page.getByTestId("toast-error")).toHaveCount(0);
  // Both rows still exist (Child may be nested, but the tree still renders both).
  await expect(rows).toHaveCount(2);
});
