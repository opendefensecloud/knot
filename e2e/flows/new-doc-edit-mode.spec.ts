/**
 * e2e: new-doc-edit-mode
 *
 * Verifies that a freshly-created document opens in edit mode via the
 * production mechanism (markDocEditMode → sessionStorage), NOT the global
 * test-suite override (localStorage["knot.editMode.defaultOn"]).
 *
 * The global override is injected by playwright.config.ts via storageState.
 * Each test here removes that key with an addInitScript so that only the
 * real production path (sessionStorage per-doc flag set by DocTree on creation)
 * is exercised.
 */
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
// Each test creates a fresh workspace via /setup, so reset before every test.
test.beforeEach(reset);

test("a newly-created doc opens in edit mode with an editable title", async ({ browser }) => {
  // Use a fresh browser context so storageState from the project config is
  // applied, then immediately override with an initScript that removes the
  // global test-suite flag before any page JS runs.
  const ctx = await browser.newContext();

  // Strip the global localStorage override BEFORE any page script runs so
  // only the production per-doc sessionStorage path is active.
  await ctx.addInitScript(() => {
    try {
      window.localStorage.removeItem("knot.editMode.defaultOn");
    } catch {
      // localStorage unavailable — no-op
    }
  });

  const page = await ctx.newPage();

  // Sign in as owner (same pattern as role-gating.spec.ts / share-link.spec.ts).
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@e.com");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  // Open the NewDocPicker and choose the "Blank document" option.
  // data-testid="new-doc-blank" is the button in NewDocPicker (DocTree.tsx ~line 228).
  await page.getByTestId("new-doc").click();
  await page.getByTestId("new-doc-blank").click();

  // Should navigate to the new doc.
  await page.waitForURL(/\/doc\/.+/);

  // toggle-edit-mode is an IconButton with active={editMode}; IconButton sets
  // aria-pressed to the active value, so "true" means edit mode is on.
  const toggle = page.getByTestId("toggle-edit-mode");
  await expect(toggle).toHaveAttribute("aria-pressed", "true", { timeout: 5_000 });

  // doc-title input should NOT be readonly in edit mode.
  const titleInput = page.getByTestId("doc-title");
  await expect(titleInput).toBeVisible({ timeout: 5_000 });
  await expect(titleInput).not.toHaveAttribute("readonly");

  await ctx.close();
});

test("title is read-only after switching to view mode", async ({ browser }) => {
  // Same override removal as the first test — each test is self-contained.
  const ctx = await browser.newContext();
  await ctx.addInitScript(() => {
    try {
      window.localStorage.removeItem("knot.editMode.defaultOn");
    } catch {
      // localStorage unavailable — no-op
    }
  });

  const page = await ctx.newPage();

  // Sign in as owner.
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@e.com");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  // Create a blank doc via the NewDocPicker.
  await page.getByTestId("new-doc").click();
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);

  // Confirm we start in edit mode (production default for new docs).
  const toggle = page.getByTestId("toggle-edit-mode");
  await expect(toggle).toHaveAttribute("aria-pressed", "true", { timeout: 5_000 });

  // Toggle to view mode.
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-pressed", "false", { timeout: 3_000 });

  // Title should now be readonly.
  const titleInput = page.getByTestId("doc-title");
  await expect(titleInput).toHaveAttribute("readonly", { timeout: 3_000 });

  await ctx.close();
});
