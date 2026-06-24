import { execSync } from "node:child_process";
import { expect, test, type Page } from "@playwright/test";

// ---------------------------------------------------------------------------
// DB reset — same table list as tree-reorder.spec.ts
// ---------------------------------------------------------------------------

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

test.beforeEach(reset);

// ---------------------------------------------------------------------------
// Auth helper — sign in as owner via /setup
// ---------------------------------------------------------------------------

async function setupOwner(page: Page) {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/, { timeout: 10_000 });
}

// ---------------------------------------------------------------------------
// Create a blank doc via the NewDocPicker and return its id
// ---------------------------------------------------------------------------

async function createBlankDoc(page: Page, title: string): Promise<string> {
  const prevUrl = page.url();

  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", {
    state: "visible",
    timeout: 5_000,
  });
  await page.getByTestId("new-doc-blank").click();

  await page.waitForURL(/\/doc\/.+/, { timeout: 10_000 });
  // If URL didn't change (already on a doc page), wait for it to differ
  if (page.url() === prevUrl) {
    await page.waitForFunction(
      (prev: string) => window.location.href !== prev,
      prevUrl,
      { timeout: 10_000 },
    );
  }

  const url = page.url();
  const id = url.split("/doc/")[1]!;

  // Set a title so we can identify the doc
  await page.locator("[data-testid='doc-title']").fill(title);
  await page.locator("[data-testid='doc-title']").blur();

  return id;
}

// ---------------------------------------------------------------------------
// Helper: inline-style paddingLeft of the div surrounding a doc-row link
// (mirrors tree-reorder.spec.ts paddingLeft helper exactly)
// ---------------------------------------------------------------------------

async function paddingLeft(page: Page, id: string): Promise<number> {
  const el = page.getByTestId(`doc-row-${id}`);
  return el.evaluate((node: Element) => {
    let cur: Element | null = node;
    while (cur) {
      const pl = (cur as HTMLElement).style?.paddingLeft;
      if (pl && pl !== "0px" && pl !== "") return parseFloat(pl);
      cur = cur.parentElement;
    }
    return 0;
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test("new doc nests under the current doc by default", async ({ page }) => {
  await setupOwner(page);

  // Create a top-level doc "Parent" and open it
  const parentId = await createBlankDoc(page, "Parent");

  // Confirm we are on the Parent doc page
  await page.waitForURL(`/doc/${parentId}`, { timeout: 5_000 });

  // Open the NewDocPicker — the Location selector should appear since a doc is open
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", {
    state: "visible",
    timeout: 5_000,
  });

  // "nested" radio should exist and be checked by default
  await expect(page.getByTestId("new-doc-loc-nested")).toBeVisible();
  await expect(page.getByTestId("new-doc-loc-nested")).toBeChecked();

  // Create the blank doc (nested by default)
  const prevUrl = page.url();
  await page.getByTestId("new-doc-blank").click();

  // Wait for navigation to the new doc
  await page.waitForURL(/\/doc\/.+/, { timeout: 10_000 });
  if (page.url() === prevUrl) {
    await page.waitForFunction(
      (prev: string) => window.location.href !== prev,
      prevUrl,
      { timeout: 10_000 },
    );
  }

  const childUrl = page.url();
  const childId = childUrl.split("/doc/")[1]!;

  // Both rows must be visible in the sidebar
  await expect(page.getByTestId(`doc-row-${parentId}`)).toBeVisible({ timeout: 8_000 });
  await expect(page.getByTestId(`doc-row-${childId}`)).toBeVisible({ timeout: 8_000 });

  // The child should be indented MORE than the parent (nested, not same-level)
  const plParent = await paddingLeft(page, parentId);
  const plChild = await paddingLeft(page, childId);
  expect(plChild).toBeGreaterThan(plParent);

  // The child row must appear AFTER the parent row in DOM (rendered in parent's subtree)
  const order = await page.evaluate(() => {
    const els = document.querySelectorAll("[data-testid^='doc-row-']");
    return Array.from(els).map(
      (el) => el.getAttribute("data-testid")!.replace("doc-row-", ""),
    );
  });
  const posParent = order.indexOf(parentId);
  const posChild = order.indexOf(childId);
  expect(posParent).toBeGreaterThanOrEqual(0);
  expect(posChild).toBeGreaterThan(posParent);
});

test("same-level creates a sibling of the current doc", async ({ page }) => {
  await setupOwner(page);

  // Create a top-level doc "Parent" and open it
  const parentId = await createBlankDoc(page, "Parent");

  // Confirm we are on the Parent doc page
  await page.waitForURL(`/doc/${parentId}`, { timeout: 5_000 });

  // Open the NewDocPicker
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", {
    state: "visible",
    timeout: 5_000,
  });

  // Switch to "same-level" (sibling)
  await page.getByTestId("new-doc-loc-sibling").click();
  await expect(page.getByTestId("new-doc-loc-sibling")).toBeChecked();

  // Create the blank doc at the same level
  const prevUrl = page.url();
  await page.getByTestId("new-doc-blank").click();

  // Wait for navigation to the new doc
  await page.waitForURL(/\/doc\/.+/, { timeout: 10_000 });
  if (page.url() === prevUrl) {
    await page.waitForFunction(
      (prev: string) => window.location.href !== prev,
      prevUrl,
      { timeout: 10_000 },
    );
  }

  const siblingUrl = page.url();
  const siblingId = siblingUrl.split("/doc/")[1]!;

  // Both rows must be visible in the sidebar
  await expect(page.getByTestId(`doc-row-${parentId}`)).toBeVisible({ timeout: 8_000 });
  await expect(page.getByTestId(`doc-row-${siblingId}`)).toBeVisible({ timeout: 8_000 });

  // Both parent and sibling should be at the SAME indentation level (both top-level)
  const plParent = await paddingLeft(page, parentId);
  const plSibling = await paddingLeft(page, siblingId);
  // Allow tiny rounding diff (same level = same paddingLeft ± 2px)
  expect(Math.abs(plSibling - plParent)).toBeLessThanOrEqual(2);
});
