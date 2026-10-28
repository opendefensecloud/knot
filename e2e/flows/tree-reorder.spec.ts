/**
 * e2e: reordering and nesting pages by dragging their sidebar rows.
 *
 * Pages are seeded through the API. Each test checks the starting order
 * there before it drags, drags the way a hand does (see tree-helpers), and
 * polls the API for the parent and the order the drop stored.
 */
import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

import {
  INDENT,
  dragRow,
  outline,
  roundTrip,
  seedPages,
  setupOwner,
  showTree,
  watchMoves,
} from "./tree-helpers";

test.beforeEach(reset);

/** Every page still has its row in the sidebar. */
async function assertAllVisible(page: Page, ids: string[]) {
  for (const id of ids) {
    await expect(page.getByTestId(`doc-row-${id}`), `doc-row-${id} should be visible`).toBeVisible();
  }
}

/** Seeds the top-level pages A, B and C, in that order, and shows them. */
async function seedABC(page: Page) {
  const { A, B, C } = await seedPages(page.request, [{ title: "A" }, { title: "B" }, { title: "C" }]);
  await showTree(page, [A, B, C]);
  expect(await outline(page.request)).toEqual(["A", "B", "C"]);
  return { A, B, C };
}

// ── 1. Reorder down ────────────────────────────────────────────────────────

test("reorder down: drag A onto the bottom quarter of C → A after C", async ({ page }) => {
  await setupOwner(page);
  const { A, B, C } = await seedABC(page);

  await dragRow(page, A, { id: C }, { zone: "below" });

  await expect.poll(() => outline(page.request)).toEqual(["B", "C", "A"]);
  await assertAllVisible(page, [A, B, C]);
});

// ── 2. Reorder up ──────────────────────────────────────────────────────────

test("reorder up: drag C onto the top quarter of A → C before A", async ({ page }) => {
  await setupOwner(page);
  const { A, B, C } = await seedABC(page);

  await dragRow(page, C, { id: A }, { zone: "above" });

  await expect.poll(() => outline(page.request)).toEqual(["C", "A", "B"]);
  await assertAllVisible(page, [A, B, C]);
});

// ── 3. Nest (into) ─────────────────────────────────────────────────────────

test("nest: drag B onto the middle of A → B becomes A's subpage", async ({ page }) => {
  await setupOwner(page);
  const { A, B, C } = await seedABC(page);

  await dragRow(page, B, { id: A }, { zone: "into" });

  await expect.poll(() => outline(page.request)).toEqual(["A", "A/B", "C"]);
  await assertAllVisible(page, [A, B, C]);
});

// ── 4. Un-nest ─────────────────────────────────────────────────────────────

test("un-nest: drag B (under A) onto the bottom quarter of C and one level left → B at the top level after C", async ({
  page,
}) => {
  await setupOwner(page);
  const { A, B, C } = await seedPages(page.request, [
    { title: "A" },
    { title: "B", parent: "A" },
    { title: "C" },
  ]);
  await showTree(page, [A, B, C]);
  expect(await outline(page.request)).toEqual(["A", "A/B", "C"]);

  // Below C, B would keep its depth and nest under C; one level left puts
  // it next to C instead.
  await dragRow(page, B, { id: C }, { zone: "below", dx: -(INDENT + 2) });

  await expect.poll(() => outline(page.request)).toEqual(["A", "C", "B"]);
  await assertAllVisible(page, [A, B, C]);
});

// ── 5. Cycle attempt ───────────────────────────────────────────────────────

test("cycle attempt: drag parent P onto its child Q → nothing moves, both visible", async ({
  page,
}) => {
  await setupOwner(page);
  const { P, Q } = await seedPages(page.request, [{ title: "P" }, { title: "Q", parent: "P" }]);
  await showTree(page, [P, Q]);
  expect(await outline(page.request)).toEqual(["P", "P/Q"]);

  // Aim at the middle of Q's row, as it was before the drag.
  const p = await page.getByTestId(`doc-row-${P}`).boundingBox();
  const q = await page.getByTestId(`doc-row-${Q}`).boundingBox();
  if (!p || !q) throw new Error("rows not laid out");
  const moves = watchMoves(page);
  await dragRow(
    page,
    P,
    { x: p.x + p.width / 2, y: q.y + q.height / 2 },
    {
      // Nothing to drop on: Q's row is gone while P is dragged.
      waitForZone: false,
      whileDragging: () => expect(page.getByTestId(`doc-row-${Q}`)).toHaveCount(0),
    },
  );

  await roundTrip(page);
  expect(moves).toEqual([]);
  await expect.poll(() => outline(page.request)).toEqual(["P", "P/Q"]);
  await assertAllVisible(page, [P, Q]);
});
