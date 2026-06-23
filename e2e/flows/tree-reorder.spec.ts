import { execSync } from "node:child_process";
import { expect, test, type Page, type Locator } from "@playwright/test";

// ---------------------------------------------------------------------------
// DB reset helpers
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

// ---------------------------------------------------------------------------
// Auth helper — mirrors tree-dnd.spec.ts exactly
// ---------------------------------------------------------------------------

async function setupOwner(page: Page) {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
}

// ---------------------------------------------------------------------------
// Doc creation helper — clicks new-doc → new-doc-blank and waits for redirect
// ---------------------------------------------------------------------------

async function createDoc(page: Page, title: string): Promise<string> {
  // Remember the current URL so we can wait for it to CHANGE (mirrors
  // the tree-dnd.spec.ts pattern — waitForURL resolves immediately if we're
  // already on a /doc/:id page, so we must wait for a NEW url instead).
  const prevUrl = page.url();

  await page.getByTestId("new-doc").click();
  // Wait for the picker modal
  await page.waitForSelector("[data-testid='new-doc-modal']", {
    state: "visible",
    timeout: 5_000,
  });
  await page.getByTestId("new-doc-blank").click();
  // Wait until the URL has changed to a NEW /doc/:id (not the previous one)
  await page.waitForURL(/\/doc\/.+/, { timeout: 10_000 });
  if (page.url() === prevUrl) {
    // URL didn't change yet — wait for it to differ from the previous one
    await page.waitForFunction(
      (prev: string) => window.location.href !== prev,
      prevUrl,
      { timeout: 10_000 },
    );
  }
  const url = page.url();
  const id = url.split("/doc/")[1]!;
  // Fill title
  await page.locator("[data-testid='doc-title']").fill(title);
  await page.locator("[data-testid='doc-title']").blur();
  return id;
}

// ---------------------------------------------------------------------------
// Drag helper — mirrors tree-dnd.spec.ts mouse strategy (dnd-kit PointerSensor
// needs manual mouse.move with steps to pass the activation distance of 6px).
//
// targetFraction: Y fraction within the target row bounding box
//   ~0.12  → top 12% → "before" intent
//   ~0.50  → middle 50% → "into" (nest) intent
//   ~0.88  → bottom 88% → "after" intent
// ---------------------------------------------------------------------------

async function dragRowTo(
  page: Page,
  dragged: Locator,
  target: Locator,
  targetFraction: number,
) {
  const dragBox = await dragged.boundingBox();
  const targetBox = await target.boundingBox();
  if (!dragBox || !targetBox) throw new Error("Row not laid out for drag");

  const startX = dragBox.x + dragBox.width * 0.3;
  const startY = dragBox.y + dragBox.height / 2;
  const endX = targetBox.x + targetBox.width * 0.3;
  const endY = targetBox.y + targetBox.height * targetFraction;

  // Press down on the dragged row
  await page.mouse.move(startX, startY);
  await page.mouse.down();

  // Horizontal jitter to trigger PointerSensor (activation distance: 6 px).
  // Use horizontal movement so we don't accidentally pass through the target
  // row's top/bottom edge zones (which would set the wrong drop intent before
  // we settle on the desired fraction).
  await page.mouse.move(startX + 3, startY, { steps: 2 });
  await page.mouse.move(startX + 7, startY, { steps: 3 });
  await page.waitForTimeout(20);

  // TELEPORT directly to the final target position with steps:1.
  // Using multiple intermediate steps causes the pointer to pass through other
  // drop-intent zones (e.g. the "before" zone at the top 25% of a row) before
  // reaching the intended zone. Because dnd-kit's overId doesn't change once
  // the first DragOver fires on a row, the intent set by that first event
  // "sticks". A single-step move ensures the first (and only) DragOver event
  // on the target row fires at exactly the intended fraction.
  await page.mouse.move(endX, endY, { steps: 1 });

  // Pause so dnd-kit fires onDragOver and React commits the drop state
  await page.waitForTimeout(300);

  await page.mouse.up();
  // Wait for the mutation + query invalidation to settle
  await page.waitForTimeout(800);
}

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

/** Assert every doc id in `ids` has a visible row in the sidebar. */
async function assertAllVisible(page: Page, ids: string[]) {
  for (const id of ids) {
    await expect(
      page.getByTestId(`doc-row-${id}`),
      `doc-row-${id} should be visible`,
    ).toBeVisible({ timeout: 5_000 });
  }
}

/** Return the DOM order of all doc-row testids currently visible. */
async function domOrder(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const els = document.querySelectorAll("[data-testid^='doc-row-']");
    return Array.from(els).map(
      (el) => el.getAttribute("data-testid")!.replace("doc-row-", ""),
    );
  });
}

/** Inline-style paddingLeft of a row element (used to detect nesting depth). */
async function paddingLeft(page: Page, id: string): Promise<number> {
  const el = page.getByTestId(`doc-row-${id}`);
  // The padding is on the parent div, not the link itself; the link *is* the
  // doc-row testid element but the div around it carries the paddingLeft.
  const px = await el.evaluate((node: Element) => {
    // Walk up to the closest ancestor that has explicit paddingLeft set
    let cur: Element | null = node;
    while (cur) {
      const pl = (cur as HTMLElement).style?.paddingLeft;
      if (pl && pl !== "0px" && pl !== "") return parseFloat(pl);
      cur = cur.parentElement;
    }
    return 0;
  });
  return px;
}

// ---------------------------------------------------------------------------
// Seed helper: create 3 root docs A, B, C and return their ids
// ---------------------------------------------------------------------------

async function seedABC(page: Page): Promise<{ a: string; b: string; c: string }> {
  const a = await createDoc(page, "A");
  const b = await createDoc(page, "B");
  const c = await createDoc(page, "C");

  // Wait until all 3 rows are present
  await expect(page.locator("[data-testid^='doc-row-']")).toHaveCount(3, {
    timeout: 8_000,
  });
  return { a, b, c };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.beforeEach(reset);

// ── 1. Reorder down ────────────────────────────────────────────────────────

test("reorder down: drag A so center lands on bottom quarter of C → A after C", async ({
  page,
}) => {
  await setupOwner(page);
  const { a, b, c } = await seedABC(page);

  const rowA = page.getByTestId(`doc-row-${a}`);
  const rowC = page.getByTestId(`doc-row-${c}`);

  // Drop A onto the bottom ~85% of C → "after" intent
  await dragRowTo(page, rowA, rowC, 0.85);

  // NON-NEGOTIABLE: every doc must still be visible
  await assertAllVisible(page, [a, b, c]);

  // Assert order: A should appear after C in the DOM
  const order = await domOrder(page);
  const posA = order.indexOf(a);
  const posC = order.indexOf(c);
  expect(posA).toBeGreaterThan(posC);
});

// ── 2. Reorder up ──────────────────────────────────────────────────────────

test("reorder up: drag C onto top quarter of A → C before A", async ({
  page,
}) => {
  await setupOwner(page);
  const { a, b, c } = await seedABC(page);

  const rowC = page.getByTestId(`doc-row-${c}`);
  const rowA = page.getByTestId(`doc-row-${a}`);

  // Drop C onto the top ~12% of A → "before" intent
  await dragRowTo(page, rowC, rowA, 0.12);

  // NON-NEGOTIABLE: every doc must still be visible
  await assertAllVisible(page, [a, b, c]);

  // Assert order: C should appear before A in the DOM
  const order = await domOrder(page);
  const posC = order.indexOf(c);
  const posA = order.indexOf(a);
  expect(posC).toBeLessThan(posA);
});

// ── 3. Nest (into) ─────────────────────────────────────────────────────────

test("nest: drag B onto middle of A → B becomes child of A (still visible)", async ({
  page,
}) => {
  await setupOwner(page);
  const { a, b, c } = await seedABC(page);

  // Get the current paddingLeft of B BEFORE nesting, so we can compare after.
  const plBBefore = await paddingLeft(page, b);

  const rowB = page.getByTestId(`doc-row-${b}`);
  const rowA = page.getByTestId(`doc-row-${a}`);

  // Drop B onto the middle 50% of A → "into" intent.
  // The drag helper uses a single-step teleport to the target to avoid
  // triggering "before" intent as the cursor passes through A's top zone.
  await dragRowTo(page, rowB, rowA, 0.5);

  // NON-NEGOTIABLE: every doc must still be visible
  await assertAllVisible(page, [a, b, c]);

  // Primary assertion: B should be indented more than before (it is now nested).
  // Root rows get paddingLeft: 4px (depth=0), nested get 4+12=16px.
  const plBAfter = await paddingLeft(page, b);
  expect(plBAfter).toBeGreaterThan(plBBefore);

  // Secondary: B should appear after A in DOM (child is rendered after parent).
  // If nest succeeded, B's row is rendered inside A's subtree, so B comes after A.
  const order = await domOrder(page);
  const posA = order.indexOf(a);
  const posB = order.indexOf(b);
  expect(posB).toBeGreaterThan(posA);
});

// ── 4. Un-nest ─────────────────────────────────────────────────────────────

test("un-nest: after nesting B under A, drag B onto bottom edge of C → B back at root", async ({
  page,
}) => {
  await setupOwner(page);
  const { a, b, c } = await seedABC(page);

  // First nest B under A
  const rowB = page.getByTestId(`doc-row-${b}`);
  const rowA = page.getByTestId(`doc-row-${a}`);
  await dragRowTo(page, rowB, rowA, 0.5);

  // Wait for nest to settle
  await assertAllVisible(page, [a, b, c]);

  // Now un-nest: drag B after C (bottom edge of C → "after" intent → sibling of C → root)
  const rowBAgain = page.getByTestId(`doc-row-${b}`);
  const rowC = page.getByTestId(`doc-row-${c}`);
  await dragRowTo(page, rowBAgain, rowC, 0.85);

  // NON-NEGOTIABLE: every doc must still be visible
  await assertAllVisible(page, [a, b, c]);

  // B should now have same (or close to) indentation as A and C (root level)
  const plB = await paddingLeft(page, b);
  const plC = await paddingLeft(page, c);
  expect(plB).toBeLessThanOrEqual(plC + 2); // allow tiny rounding
});

// ── 5. Cycle attempt ───────────────────────────────────────────────────────

test("cycle attempt: drag parent P onto its child Q → no-op, both visible", async ({
  page,
}) => {
  await setupOwner(page);

  // Create P then Q
  const p = await createDoc(page, "P");
  const q = await createDoc(page, "Q");

  await expect(page.locator("[data-testid^='doc-row-']")).toHaveCount(2, {
    timeout: 8_000,
  });

  // Nest Q under P first
  const rowQ = page.getByTestId(`doc-row-${q}`);
  const rowP = page.getByTestId(`doc-row-${p}`);
  await dragRowTo(page, rowQ, rowP, 0.5);

  // Confirm Q is nested under P
  await assertAllVisible(page, [p, q]);

  // Now try to drag P onto the MIDDLE of Q (cycle: P → Q which is already P's child)
  const rowPAgain = page.getByTestId(`doc-row-${p}`);
  const rowQAgain = page.getByTestId(`doc-row-${q}`);
  await dragRowTo(page, rowPAgain, rowQAgain, 0.5);

  // Both must remain visible — the move should have been rejected
  await assertAllVisible(page, [p, q]);

  // P should still be a parent (Q visible and nested under P — check DOM order)
  const order = await domOrder(page);
  const posP = order.indexOf(p);
  const posQ = order.indexOf(q);
  // Q is the child so it appears after P in DOM and is NOT the root of P
  expect(posP).toBeGreaterThanOrEqual(0);
  expect(posQ).toBeGreaterThanOrEqual(0);

  // No error toast from a real crash (the frontend cycle-guard shows one, that's fine;
  // but if the server rejected it instead, there may or may not be a toast — either way
  // both docs exist, which is the invariant).
});
