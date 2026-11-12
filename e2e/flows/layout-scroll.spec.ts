/**
 * e2e: only the panes scroll, never the window (#87).
 *
 * The shell is exactly the window: the sidebar and the content each scroll
 * on their own. A visually hidden (sr-only) live region is absolutely
 * positioned; inside a pane with no positioned ancestor of its own it
 * resolved against the shell instead, sat at the foot of the pane's whole,
 * unscrolled content and overflowed the window, which then grew a third
 * scrollbar that moved both panes up into empty space.
 *
 * doc-dock.spec.ts covers the case that caused #87: a long document and the
 * edit-mode announcer inside <main>. This spec covers the sidebar, which is
 * hardened the same way although nothing in it overflowed: the tree's own
 * live region is a child of a flex column, and an absolutely positioned flex
 * child sits where it would as the only item, at the top, not at the foot of
 * a tall tree. So the test plants such an element itself, a visually hidden
 * probe at the end of the tree's block-level list, and checks that the
 * sidebar holds it: without the fix the probe overflowed the window by
 * about the height of the tree. The document is too short to scroll at all,
 * so whatever could overflow the window comes from the sidebar.
 */
import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

import { seedPages, type PageSpec } from "./tree-helpers";

// Reset once: the owner is created in the beforeAll below, and a per-test
// reset would delete it out from under the later tests.
test.beforeAll(reset);

const EMAIL = "owner@layout.test";
const PASSWORD = "hunter22!layout";

test.beforeAll(async () => {
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD, display_name: "Owner" }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});

async function signIn(page: Page) {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
}

/** Root pages enough for a tree well past the 720px of Desktop Chrome's
 *  viewport, at a row or so of 28px each. */
const TREE_PAGES = 40;

/** How far the window itself could scroll down: the root's scrollable
 *  overflow past the viewport. Zero or less means the window has no
 *  scrollbar of its own and only the panes inside it scroll. */
async function windowOverflow(page: Page): Promise<number> {
  return page.evaluate(() => {
    const root = document.scrollingElement!;
    return root.scrollHeight - root.clientHeight;
  });
}

test("a tree taller than the window scrolls inside the sidebar; the window itself never scrolls", async ({
  page,
}) => {
  await signIn(page);
  const specs: PageSpec<string>[] = Array.from({ length: TREE_PAGES }, (_, i) => ({
    title: `Tree page ${String(i + 1).padStart(2, "0")}`,
  }));
  const ids = await seedPages(page.request, specs);
  const firstId = ids[specs[0]!.title]!;
  const lastId = ids[specs[TREE_PAGES - 1]!.title]!;

  // A page with nothing in it, so the document's own announcer sits near
  // the top and whatever overflows the window comes from the sidebar.
  await page.goto(`/doc/${firstId}`);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
  await expect(page.getByTestId(`doc-row-${lastId}`)).toBeAttached({ timeout: 10_000 });

  // The premise: the tree really is taller than the sidebar.
  const sidebarOverflow = await page.evaluate(() => {
    const aside = document.querySelector("aside")!;
    return aside.scrollHeight - aside.clientHeight;
  });
  expect(sidebarOverflow).toBeGreaterThan(0);

  // The probe: an absolutely positioned element with no positioned host of
  // its own at the foot of the tree, as a future live region or tooltip
  // could be. Inside the list rather than straight in the flex column, where
  // it would sit at the top. A root row's list is the tree's own, not the
  // nested "Shared with you" one.
  const probeHost = await page.evaluate(() => {
    const treeList = document.querySelector('[data-testid^="doc-row-"]')!.closest("ul")!;
    const probe = document.createElement("span");
    probe.className = "sr-only";
    probe.textContent = "probe";
    treeList.append(probe);
    // A positioned element's offsetParent is its nearest positioned
    // ancestor: the box it resolves against.
    return (probe.offsetParent as HTMLElement | null)?.tagName;
  });
  // The sidebar is the probe's containing block, so it scrolls and clips it.
  // This is what pins the fix: the shell's overflow-clip backstop alone would
  // already keep the window checks below green.
  expect(probeHost).toBe("ASIDE");

  expect(await windowOverflow(page)).toBeLessThanOrEqual(0);

  // Scrolled to its foot, the tree still scrolls only itself, and the last
  // page comes into view inside the sidebar.
  await page.evaluate(() => {
    const aside = document.querySelector("aside")!;
    aside.scrollTop = aside.scrollHeight;
  });
  await expect(page.getByTestId(`doc-row-${lastId}`)).toBeInViewport();
  expect(await windowOverflow(page)).toBeLessThanOrEqual(0);

  // Asked to scroll anyway, the window stays put. scrollTo rather than the
  // mouse wheel: a wheel lands in a pane first and reaches the window only
  // once the pane has run out.
  await page.evaluate(() => window.scrollTo(0, 1e6));
  expect(await page.evaluate(() => window.scrollY)).toBe(0);
});
