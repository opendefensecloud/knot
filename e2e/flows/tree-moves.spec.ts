/**
 * e2e: moving pages in the sidebar by depth (dragging left and right), into
 * a collapsed page, to the very top, from the row menu and with Alt+arrow
 * keys; and drags that must change nothing.
 *
 * Pages are seeded through the API and every result is polled from it (see
 * tree-helpers). A test that expects no move also checks that the page sent
 * no move request at all.
 */
import { expect, test } from "@playwright/test";

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

/** What a test plants in the page to watch it from outside. */
type Probe = Window & { __altArrowsCancelled?: boolean[] };

test("un_nest_last_child_of_last_root_by_dragging_left", async ({ page }) => {
  await setupOwner(page);
  const { A, R, C } = await seedPages(page.request, [
    { title: "A" },
    { title: "R" },
    { title: "C", parent: "R" },
  ]);
  await showTree(page, [A, R, C]);
  expect(await outline(page.request)).toEqual(["A", "R", "R/C"]);

  // C is the sidebar's last row: there is no row after R's subtree to drop
  // it on. One level left on its own row puts it right after R.
  await dragRow(page, C, { id: C }, { zone: "below", dx: -(INDENT + 2) });

  await expect.poll(() => outline(page.request)).toEqual(["A", "R", "C"]);
});

test("nest_by_dragging_right", async ({ page }) => {
  await setupOwner(page);
  const { A, B } = await seedPages(page.request, [{ title: "A" }, { title: "B" }]);
  await showTree(page, [A, B]);
  expect(await outline(page.request)).toEqual(["A", "B"]);

  // One level right on its own row: B becomes the subpage of A, the page above.
  await dragRow(page, B, { id: B }, { zone: "below", dx: INDENT + 2 });

  await expect.poll(() => outline(page.request)).toEqual(["A", "A/B"]);
});

test("drop_into_collapsed_parent", async ({ page }) => {
  await setupOwner(page);
  const { P, K, X } = await seedPages(page.request, [
    { title: "P" },
    { title: "K", parent: "P" },
    { title: "X" },
  ]);
  await showTree(page, [P, K, X]);
  expect(await outline(page.request)).toEqual(["P", "P/K", "X"]);

  // The chevron sits next to the link, in the same row.
  await page
    .getByTestId(`doc-row-${P}`)
    .locator("..")
    .getByRole("button", { name: "Collapse" })
    .click();
  await expect(page.getByTestId(`doc-row-${K}`)).toHaveCount(0);

  await dragRow(page, X, { id: P }, { zone: "into" });

  // Into a page means after its last subpage, shown or not.
  await expect.poll(() => outline(page.request)).toEqual(["P", "P/K", "P/X"]);
});

test("menu_indent_and_outdent", async ({ page }) => {
  await setupOwner(page);
  const { A, B } = await seedPages(page.request, [{ title: "A" }, { title: "B" }]);
  await showTree(page, [A, B]);
  expect(await outline(page.request)).toEqual(["A", "B"]);

  await page.getByTestId(`doc-row-${B}`).click({ button: "right" });
  await page.getByTestId("ctx-indent").click();
  await expect.poll(() => outline(page.request)).toEqual(["A", "A/B"]);

  await page.getByTestId(`doc-row-${B}`).click({ button: "right" });
  await page.getByTestId("ctx-outdent").click();
  await expect.poll(() => outline(page.request)).toEqual(["A", "B"]);
});

// Review Focus 4: in Windows and Linux browsers Alt+← is Back and Alt+→ is
// Forward. The row must take the keys: the move happens, and the browser
// doesn't navigate.
test("alt_arrow_indents_without_navigating", async ({ page }) => {
  await setupOwner(page);
  const { A, B } = await seedPages(page.request, [{ title: "A" }, { title: "B" }]);
  // A's page with history on both sides, so either key would leave it.
  await page.goto(`/doc/${B}`);
  await page.goto(`/doc/${A}`);
  await page.goto(`/doc/${B}`);
  await page.goBack();
  const onA = new RegExp(`/doc/${A}$`);
  await expect(page).toHaveURL(onA);
  await expect(page.getByTestId(`doc-row-${B}`)).toBeVisible({ timeout: 10_000 });
  expect(await outline(page.request)).toEqual(["A", "B"]);

  // Whether the keys were cancelled, as the window sees them: after the row.
  await page.evaluate(() => {
    const probe = window as Probe;
    probe.__altArrowsCancelled = [];
    window.addEventListener("keydown", (e) => {
      if (e.altKey && e.key.startsWith("Arrow")) probe.__altArrowsCancelled!.push(e.defaultPrevented);
    });
  });

  await page.getByTestId(`doc-row-${B}`).focus();
  await page.keyboard.press("Alt+ArrowRight");
  await expect.poll(() => outline(page.request)).toEqual(["A", "A/B"]);
  await expect(page).toHaveURL(onA);
  // Under its new parent B's row is a new element, and it keeps the focus.
  await expect(page.getByTestId(`doc-row-${B}`)).toBeFocused();

  await page.getByTestId(`doc-row-${B}`).focus();
  await page.keyboard.press("Alt+ArrowLeft");
  await expect.poll(() => outline(page.request)).toEqual(["A", "B"]);
  await expect(page).toHaveURL(onA);

  expect(await page.evaluate(() => (window as Probe).__altArrowsCancelled)).toEqual([true, true]);
});

test("releasing_on_own_row_does_not_reload", async ({ page }) => {
  await setupOwner(page);
  const { A, B } = await seedPages(page.request, [{ title: "A" }, { title: "B" }]);
  let loads = 0;
  page.on("load", () => loads++);
  await showTree(page, [A, B], `/doc/${A}`);
  expect(await outline(page.request)).toEqual(["A", "B"]);

  // A reload would start in the click that ends the drag, on B's link.
  // beforeunload runs before the page goes, and sessionStorage outlives it.
  await page.evaluate(() =>
    window.addEventListener("beforeunload", () => sessionStorage.setItem("tree-e2e-unload", "1")),
  );
  const moves = watchMoves(page);

  // Off and back onto B's own row, at its depth: nothing to drop. Off onto
  // A's row first: a drag that travels less than one INDENT is a click,
  // which opens the page.
  const b = await page.getByTestId(`doc-row-${B}`).boundingBox();
  if (!b) throw new Error("not laid out");
  await dragRow(page, B, { id: B }, {
    zone: "below",
    waitForZone: false,
    whileDragging: () => page.mouse.move(b.x + b.width / 2, b.y - b.height / 2, { steps: 5 }),
  });

  expect(await page.evaluate(() => sessionStorage.getItem("tree-e2e-unload"))).toBeNull();
  await roundTrip(page);
  expect(loads).toBe(1);
  await expect(page).toHaveURL(new RegExp(`/doc/${A}$`));
  expect(moves).toEqual([]);
  expect(await outline(page.request)).toEqual(["A", "B"]);
});

// A trackpad click easily slips a few px. Past the 6px that start a drag,
// but short of one INDENT, it moves nothing and opens the page in the app,
// with no page load. The browser's own click can't do it: the button goes up
// over the drag ghost, so that click never reaches the link.
test("a_click_that_slips_sideways_opens_the_page", async ({ page }) => {
  await setupOwner(page);
  const { A, A1, B } = await seedPages(page.request, [
    { title: "A" },
    { title: "A1", parent: "A" },
    { title: "B" },
  ]);
  let loads = 0;
  page.on("load", () => loads++);
  await showTree(page, [A, A1, B], `/doc/${A}`);
  const moves = watchMoves(page);

  // 9px right on B, which one INDENT would nest under A; 9px left on A1, a
  // last child, which one INDENT would un-nest.
  for (const [id, dx] of [
    [B, 9],
    [A1, -9],
  ] as const) {
    const link = await page.getByTestId(`doc-row-${id}`).boundingBox();
    if (!link) throw new Error("not laid out");
    const x = link.x + 30;
    const y = link.y + link.height / 2;
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x + dx, y + 1, { steps: 4 });
    await expect(page.getByTestId(`tree-row-${id}`)).toHaveCSS("opacity", "0.5"); // dragging
    await page.mouse.up();
    await expect(page).toHaveURL(new RegExp(`/doc/${id}$`));
  }

  await roundTrip(page);
  expect(moves).toEqual([]);
  expect(loads).toBe(1);
  expect(await outline(page.request)).toEqual(["A", "A/A1", "B"]);
});

// Collision is by pointer: a click pressed at the top edge of B's row that
// slips a few px up is in the middle of A's row, which nests B. A drop
// after less than one INDENT of travel moves nothing, and opens the page as
// the click would (ruling F6).
test("a_click_that_slips_up_into_the_row_above_opens_the_page", async ({ page }) => {
  await setupOwner(page);
  const { A, B } = await seedPages(page.request, [{ title: "A" }, { title: "B" }]);
  let loads = 0;
  page.on("load", () => loads++);
  await showTree(page, [A, B], `/doc/${A}`);
  const moves = watchMoves(page);

  const link = await page.getByTestId(`doc-row-${B}`).boundingBox();
  const above = await page.getByTestId(`tree-row-${A}`).boundingBox();
  if (!link || !above) throw new Error("not laid out");
  const x = link.x + 30;
  // 1px into B's link, and up to A's middle band, which reaches down to
  // three quarters of the row: past the 6px that start a drag, short of
  // one INDENT.
  const from = link.y + 1;
  const to = above.y + above.height * 0.7;
  expect(from - to).toBeGreaterThan(6);
  expect(from - to).toBeLessThan(INDENT);
  await page.mouse.move(x, from);
  await page.mouse.down();
  await page.mouse.move(x, to, { steps: 4 });
  await expect(page.getByTestId(`tree-row-${B}`)).toHaveCSS("opacity", "0.5"); // dragging
  await expect(page.locator("[data-testid^='tree-row-'][data-drop-zone]")).toHaveCount(0);
  await page.mouse.up();
  await expect(page).toHaveURL(new RegExp(`/doc/${B}$`));

  await roundTrip(page);
  expect(moves).toEqual([]);
  expect(loads).toBe(1);
  expect(await outline(page.request)).toEqual(["A", "B"]);
});

test("drop_outside_cancels", async ({ page }) => {
  await setupOwner(page);
  const { A, B, C } = await seedPages(page.request, [{ title: "A" }, { title: "B" }, { title: "C" }]);
  await showTree(page, [A, B, C], `/doc/${A}`);
  expect(await outline(page.request)).toEqual(["A", "B", "C"]);

  // In the editor pane, below the page's title and level with B's row: well
  // right of the tree, and nowhere near the band above its first row that
  // still counts as the tree. Within the tree, that height nests A into B.
  const title = await page.getByTestId("doc-title").boundingBox();
  const b = await page.getByTestId(`doc-row-${B}`).boundingBox();
  if (!title || !b) throw new Error("not laid out");
  const moves = watchMoves(page);
  await dragRow(
    page,
    A,
    { x: title.x + title.width / 2, y: b.y + b.height / 2 },
    { waitForZone: false },
  );

  await roundTrip(page);
  expect(moves).toEqual([]);
  expect(await outline(page.request)).toEqual(["A", "B", "C"]);
  for (const id of [A, B, C]) await expect(page.getByTestId(`doc-row-${id}`)).toBeVisible();
});

test("both_rows_render_after_moves", async ({ page }) => {
  await setupOwner(page);
  const { Parent, Child } = await seedPages(page.request, [{ title: "Parent" }, { title: "Child" }]);
  await showTree(page, [Parent, Child]);
  expect(await outline(page.request)).toEqual(["Parent", "Child"]);
  const rows = page.locator("[data-testid^='doc-row-']");

  await dragRow(page, Child, { id: Parent }, { zone: "into" });
  await expect.poll(() => outline(page.request)).toEqual(["Parent", "Parent/Child"]);
  await expect(rows).toHaveCount(2);
  await expect(page.getByTestId(`doc-row-${Parent}`)).toBeVisible();
  await expect(page.getByTestId(`doc-row-${Child}`)).toBeVisible();

  await dragRow(page, Child, { id: Child }, { zone: "below", dx: -(INDENT + 2) });
  await expect.poll(() => outline(page.request)).toEqual(["Parent", "Child"]);
  await expect(rows).toHaveCount(2);
  await expect(page.getByTestId(`doc-row-${Parent}`)).toBeVisible();
  await expect(page.getByTestId(`doc-row-${Child}`)).toBeVisible();

  await expect(page.getByTestId("toast-error")).toHaveCount(0);
});

test("move_to_first_position", async ({ page }) => {
  await setupOwner(page);
  const { A, B, C } = await seedPages(page.request, [{ title: "A" }, { title: "B" }, { title: "C" }]);
  await showTree(page, [A, B, C]);
  expect(await outline(page.request)).toEqual(["A", "B", "C"]);

  // A few px above the first row: the band one row high above the list
  // counts as the gap above that row.
  const first = await page.getByTestId(`doc-row-${A}`).boundingBox();
  if (!first) throw new Error("first row not laid out");
  await dragRow(page, C, { x: first.x + first.width / 2, y: first.y - 4 }, { zone: "above" });

  await expect.poll(() => outline(page.request)).toEqual(["C", "A", "B"]);
});
