/**
 * e2e: the left navigation sidebar is resizable.
 *
 * What it pins:
 *
 *  - a vertical separator (data-testid="sidebar-resize-handle") on the
 *    sidebar's right edge, on tablet/desktop only; the phone drawer keeps
 *    its fixed 260px;
 *  - the rendered width is clamp(preference, 200, maxFor(viewport)), where
 *    maxFor(V) = max(200, min(480, V - 380)), so the content column never
 *    drops below 380px; 260px stays the default;
 *  - drag, double-click (reset to 260) and the keyboard all resize; the
 *    preference is saved as integer px under localStorage
 *    "knot.sidebarWidth" and mirrored into other tabs;
 *  - the comment-rail inset in layout.css keys off the real content width
 *    (viewport - sidebar >= 1020px) instead of a bare 1280px breakpoint.
 *
 * Unless a test says otherwise it runs at Desktop Chrome's 1280x720, where
 * maxFor(1280) = min(480, 900) = 480.
 */
import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

// Reset once: the owner is created in the beforeAll below, and a per-test
// reset would delete it out from under the later tests.
test.beforeAll(reset);

const EMAIL = "owner@sidebar.test";
const PASSWORD = "hunter22!sidebar";

// /setup renders its form whether or not an owner exists, so probing the
// page to decide between setup and login doesn't work. Create the owner
// once against the API instead, and have every test sign in normally.
test.beforeAll(async () => {
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD, display_name: "Owner" }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});

/** Where the preference lives: the width in whole px, as a string. */
const KEY = "knot.sidebarWidth";

/** Globals the tests plant in a page so they can observe it from outside. */
type Probe = Window & {
  __firstSidebarWidth?: number;
  __sidebarEvents?: (string | null)[];
  __sameDocument?: boolean;
};

/** Each test gets a fresh browser context, so every one needs a session. */
async function signIn(page: Page) {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
}

async function newDoc(page: Page) {
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
}

const sidebar = (page: Page) => page.getByTestId("sidebar");
const handle = (page: Page) => page.getByTestId("sidebar-resize-handle");

/** Rendered width of the sidebar's border box. */
async function sidebarWidth(page: Page) {
  const box = await sidebar(page).boundingBox();
  if (!box) throw new Error("the sidebar is not rendered");
  return box.width;
}

/** Exact width, for keyboard, reset, clamp and restore — all whole px.
 *  Polled, so the check does not depend on when or how the new width is
 *  applied. */
async function expectWidth(page: Page, px: number, context = "sidebar width") {
  await expect.poll(() => sidebarWidth(page), { message: `${context}: expected ${px}px` }).toBe(px);
}

/** A drag's width, give or take the 1px a sub-pixel grab point can cost. */
async function expectWidthNear(page: Page, px: number) {
  const message = `sidebar width: expected ${px}px ±1`;
  await expect.poll(() => sidebarWidth(page), { message }).toBeGreaterThanOrEqual(px - 1);
  await expect.poll(() => sidebarWidth(page), { message }).toBeLessThanOrEqual(px + 1);
}

function storedWidth(page: Page) {
  return page.evaluate((k) => localStorage.getItem(k), KEY);
}

/** Leave a preference the way an earlier session would have, then reload
 *  so the app boots with it. */
async function seedSavedWidth(page: Page, value: string) {
  await page.evaluate(([k, v]) => localStorage.setItem(k, v), [KEY, value] as const);
  await page.reload();
}

/** Put the pointer on the handle's centre and press the primary button,
 *  leaving it held. The visibility check first makes a missing handle fail
 *  in 5s with a clear message, instead of boundingBox() waiting out the
 *  whole test timeout. */
async function grab(page: Page) {
  await expect(handle(page)).toBeVisible();
  const box = (await handle(page).boundingBox())!;
  const at = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
  await page.mouse.move(at.x, at.y);
  await page.mouse.down();
  return at;
}

async function focusHandle(page: Page) {
  await expect(handle(page)).toBeVisible();
  await handle(page).focus();
  await expect(handle(page)).toBeFocused();
}

function selectedText(page: Page) {
  return page.evaluate(() => window.getSelection()?.toString() ?? "");
}

/** Cursor and user-select on <html> and <body>: where a drag would put its
 *  page-wide overrides. */
function rootStyles(page: Page) {
  return page.evaluate(() =>
    [document.documentElement, document.body].map((el) => {
      const s = getComputedStyle(el);
      return { cursor: s.cursor, userSelect: s.getPropertyValue("user-select") };
    }),
  );
}

function docPaddingRight(page: Page) {
  return page.getByTestId("doc-page").evaluate((el) => getComputedStyle(el).paddingRight);
}

test("by default the sidebar is 260px wide, with an accessible separator to resize it", async ({ page }) => {
  await signIn(page);
  await expectWidth(page, 260);

  const sep = page.getByRole("separator", { name: "Resize sidebar", exact: true });
  await expect(sep).toBeVisible();
  await expect(sep).toHaveAttribute("data-testid", "sidebar-resize-handle");
  await expect(sep).toHaveAttribute("aria-orientation", "vertical");
  await expect(sep).toHaveAttribute("tabindex", "0");
  await expect(sep).toHaveAttribute("aria-valuenow", "260");
  await expect(sep).toHaveAttribute("aria-valuemin", "200");
  // maxFor(1280) = min(480, 1280 - 380).
  await expect(sep).toHaveAttribute("aria-valuemax", "480");
  // aria-controls names the element the separator resizes.
  await expect(sidebar(page)).toHaveAttribute("id", "app-sidebar");
  await expect(sep).toHaveAttribute("aria-controls", "app-sidebar");
});

test("the handle straddles the sidebar's right edge, full height, with a col-resize cursor", async ({ page }) => {
  await signIn(page);
  await expectWidth(page, 260);
  await expect(handle(page)).toBeVisible();

  const aside = (await sidebar(page).boundingBox())!;
  const h = (await handle(page).boundingBox())!;
  const edge = aside.x + aside.width;
  // It reaches a little way into the sidebar — not far, the sidebar's
  // scrollbar lives there — and out past the 1px border into the content.
  expect(h.x, "handle's left side").toBeLessThan(edge);
  expect(h.x, "handle's left side").toBeGreaterThanOrEqual(edge - 5);
  expect(h.x + h.width, "handle's right side").toBeGreaterThan(edge);
  // A comfortable ~8px hit area: not a hairline, not a slab.
  expect(h.width, "handle's hit area").toBeGreaterThanOrEqual(4);
  expect(h.width, "handle's hit area").toBeLessThanOrEqual(16);
  // Full height, so it can be grabbed anywhere along the edge.
  expect(h.y, "handle's top").toBeLessThanOrEqual(aside.y + 1);
  expect(h.y + h.height, "handle's bottom").toBeGreaterThanOrEqual(aside.y + aside.height - 1);

  await expect(handle(page)).toHaveCSS("cursor", "col-resize");
  // Without it, a touch drag on a tablet pans the page instead of resizing.
  await expect(handle(page)).toHaveCSS("touch-action", "none");
});

test("the handle keeps its pixel geometry at a larger browser font size", async ({ page }) => {
  await signIn(page);
  await expectWidth(page, 260);
  await expect(handle(page)).toBeVisible();
  // A low-vision user's "Very large" browser font: rem follows the root font
  // size, px does not, and the edge it has to line up with is in px.
  await page.evaluate(() => {
    document.documentElement.style.fontSize = "24px";
  });

  const aside = (await sidebar(page).boundingBox())!;
  const edge = aside.x + aside.width;
  const h = (await handle(page).boundingBox())!;
  expect(h.width, "handle's hit area").toBe(8);
  expect(h.x, "handle's left side").toBe(edge - 3);
  // The accent line sits exactly over the sidebar's 1px border, not beside it.
  await handle(page).hover();
  const line = (await handle(page).locator("span").boundingBox())!;
  expect(line.x, "accent line's left side").toBe(edge - 1);
  expect(line.width, "accent line's width").toBe(2);
});

test("the sidebar's own controls keep clear of the handle's target area", async ({ page }) => {
  await signIn(page);
  await newDoc(page);
  // A row in the tree, which is a drag target of its own.
  await expect(sidebar(page).locator("li[role='button']").first()).toBeVisible();
  await expectWidth(page, 260);
  await expect(handle(page)).toBeVisible();

  // The 8px handle is smaller than WCAG 2.5.8's 24px, which it may be only
  // while no other target sits inside the 24px circle centred on it. Rows
  // can sit at any height along the edge, so every one keeps 12px clear of
  // the handle's centre line. A near miss then lands on nothing, rather than
  // starting a document drag.
  const h = (await handle(page).boundingBox())!;
  const centre = h.x + h.width / 2;
  const rights = await sidebar(page)
    .locator("a, button, input, [role='button'], [tabindex]:not([tabindex='-1'])")
    .evaluateAll((els) =>
      els
        .map((el) => el.getBoundingClientRect())
        .filter((r) => r.width > 0 && r.height > 0)
        .map((r) => r.right),
    );
  expect(rights.length, "targets found in the sidebar").toBeGreaterThan(0);
  expect(Math.max(...rights), "rightmost target in the sidebar").toBeLessThanOrEqual(centre - 12);
});

test("dragging the handle widens the sidebar 1:1, and the width survives a reload", async ({ page }) => {
  await signIn(page);
  await expectWidth(page, 260);

  const { x, y } = await grab(page);
  await page.mouse.move(x + 80, y, { steps: 10 });
  // The edge follows the pointer live, while the button is still down...
  await expectWidthNear(page, 340);
  // ...but nothing is saved until release: a drag commits once.
  expect(await storedWidth(page), "saved width mid-drag").toBeNull();
  await page.mouse.up();

  await expect.poll(() => storedWidth(page)).toEqual(expect.stringMatching(/^\d+$/));
  const saved = Number(await storedWidth(page));
  expect(saved, "saved width").toBeGreaterThanOrEqual(339);
  expect(saved, "saved width").toBeLessThanOrEqual(341);
  // What is saved is exactly what is shown.
  expect(await sidebarWidth(page)).toBe(saved);

  await page.reload();
  await expectWidth(page, saved, "sidebar width after reload");
  await expect(handle(page)).toHaveAttribute("aria-valuenow", String(saved));
});

test("grabbing the handle off-centre does not make the edge jump", async ({ page }) => {
  await signIn(page);
  await expectWidth(page, 260);
  await expect(handle(page)).toBeVisible();

  // 1px inside the hit area's outer side, a few px right of the visible
  // edge. An implementation that snapped the edge to the pointer would
  // widen the sidebar on the press alone, then stay that far off.
  const box = (await handle(page).boundingBox())!;
  const x = box.x + box.width - 1;
  const y = box.y + box.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await expectWidth(page, 260, "sidebar width on press");
  await page.mouse.move(x + 40, y, { steps: 8 });
  await expectWidthNear(page, 300);
  await page.mouse.up();
  await expectWidthNear(page, 300);
});

test("a drag stops at 200px on the left and 480px on the right", async ({ page }) => {
  await signIn(page);
  await expectWidth(page, 260);

  // Far left: the pointer ends deep inside the sidebar, well short of 200.
  const left = await grab(page);
  await page.mouse.move(5, left.y, { steps: 10 });
  await expectWidth(page, 200, "sidebar width dragged far left");
  await page.mouse.up();
  await expectWidth(page, 200, "sidebar width after release");
  await expect(handle(page)).toHaveAttribute("aria-valuenow", "200");

  // Far right: maxFor(1280) = 480, though the pointer ends at 1200.
  const right = await grab(page);
  await page.mouse.move(1200, right.y, { steps: 10 });
  await expectWidth(page, 480, "sidebar width dragged far right");
  await page.mouse.up();
  await expectWidth(page, 480, "sidebar width after release");
  await expect(handle(page)).toHaveAttribute("aria-valuenow", "480");
  await expect.poll(() => storedWidth(page)).toBe("480");
});

test("a drag selects no text, and leaves the cursor and user-select as it found them", async ({ page }) => {
  await signIn(page);
  const label = sidebar(page).getByText("Documents", { exact: true });
  await expect(label).toBeVisible();
  const before = await rootStyles(page);
  await page.evaluate(() => window.getSelection()?.removeAllRanges());

  await grab(page);
  // Sweep back across the sidebar's own text, up to the "Documents" label.
  // A drag that leaked into a text selection would highlight it on the way.
  const to = (await label.boundingBox())!;
  await page.mouse.move(to.x + to.width / 2, to.y + to.height / 2, { steps: 12 });
  expect(await selectedText(page), "selected text mid-drag").toBe("");
  await page.mouse.up();
  expect(await selectedText(page), "selected text after the drag").toBe("");

  // A drag may override both page-wide while it runs; release must undo
  // that, or the app is left with a resize cursor and unselectable text.
  await expect.poll(() => rootStyles(page)).toEqual(before);
});

test("double-clicking the handle resets the width to 260px, and that survives a reload", async ({ page }) => {
  await signIn(page);
  await seedSavedWidth(page, "400");
  await expectWidth(page, 400, "sidebar width from a saved 400");

  await expect(handle(page)).toBeVisible();
  await handle(page).dblclick();
  await expectWidth(page, 260, "sidebar width after double-click");
  // The reset is saved like any other width. (Asserted directly: after a
  // reload 260 would also be what a forgotten save looks like.)
  await expect.poll(() => storedWidth(page)).toBe("260");

  await page.reload();
  await expectWidth(page, 260, "sidebar width after reload");
});

test("dragging or double-clicking the edge leaves focus, and the caret, in the editor", async ({ page }) => {
  await signIn(page);
  await newDoc(page);
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("Hello");

  const { x, y } = await grab(page);
  await page.mouse.move(x + 60, y, { steps: 6 });
  await page.mouse.up();
  await expectWidthNear(page, 320);
  await expect(editor, "focus after a drag").toBeFocused();
  // Typing carries on where it left off...
  await page.keyboard.type(" world");
  await expect(editor).toHaveText("Hello world");
  // ...and the arrow keys move the caret, not the edge.
  const saved = await storedWidth(page);
  await page.keyboard.press("ArrowLeft");
  await page.keyboard.type("!");
  await expect(editor).toHaveText("Hello worl!d");
  expect(await storedWidth(page), "saved width after ArrowLeft in the editor").toBe(saved);

  await handle(page).dblclick();
  await expectWidth(page, 260, "sidebar width after double-click");
  await expect(editor, "focus after a double-click").toBeFocused();
  await page.keyboard.type("?");
  await expect(editor).toHaveText("Hello worl!?d");
});

test("the keyboard resizes in 16px steps (64 with Shift), and Home / End jump to the limits", async ({ page }) => {
  await signIn(page);
  await expectWidth(page, 260);
  await focusHandle(page);
  // Keep the pointer off the edge, so hover cannot stand in for focus below.
  await page.mouse.move(800, 400);

  const presses: [key: string, px: number][] = [
    ["ArrowRight", 276],
    ["Shift+ArrowRight", 340],
    ["ArrowLeft", 324],
    ["Shift+ArrowLeft", 260],
    ["Home", 200],
    ["ArrowLeft", 200], // already at the minimum
    ["End", 480],
    ["ArrowRight", 480], // already at maxFor(1280)
  ];
  for (const [key, px] of presses) {
    await page.keyboard.press(key);
    await expect(handle(page), `aria-valuenow after ${key}`).toHaveAttribute("aria-valuenow", String(px));
    await expectWidth(page, px, `sidebar width after ${key}`);
    // Every press is saved, not only the last one.
    await expect.poll(() => storedWidth(page), { message: `saved width after ${key}` }).toBe(String(px));
  }

  // The accent line is the separator's only focus indicator: lit while the
  // keyboard is on the edge...
  const line = handle(page).locator("span");
  await expect(line, "accent line with keyboard focus").toHaveCSS("opacity", "1");

  // Keys the handle doesn't use are left alone: Tab still moves focus on.
  await page.keyboard.press("Tab");
  await expect(handle(page)).not.toBeFocused();
  // ...and gone once focus has moved on.
  await expect(line, "accent line after focus moved on").toHaveCSS("opacity", "0");
});

test("narrowing the window clamps the sidebar but keeps the saved preference", async ({ page }) => {
  await signIn(page);
  await seedSavedWidth(page, "480");
  await expectWidth(page, 480, "sidebar width from a saved 480");

  // maxFor(800) = min(480, 800 - 380) = 420: the content keeps its 380px.
  await page.setViewportSize({ width: 800, height: 720 });
  await expectWidth(page, 420, "sidebar width at 800px");
  await expect(handle(page)).toHaveAttribute("aria-valuemax", "420");
  await expect(handle(page)).toHaveAttribute("aria-valuenow", "420");
  // The clamp is a rendering concern; the preference itself is untouched...
  expect(await storedWidth(page)).toBe("480");

  // ...so widening the window brings it back.
  await page.setViewportSize({ width: 1280, height: 720 });
  await expectWidth(page, 480, "sidebar width back at 1280px");
  await expect(handle(page)).toHaveAttribute("aria-valuemax", "480");
});

test("after a drag, the window still clamps the sidebar", async ({ page }) => {
  await signIn(page);
  await expectWidth(page, 260);

  // A drag may hold the column at its own width while it lasts...
  const { x, y } = await grab(page);
  await page.mouse.move(x + 400, y, { steps: 10 });
  await expectWidth(page, 480, "sidebar width dragged far right");
  await page.mouse.up();
  await expectWidth(page, 480, "sidebar width after release");

  // ...but once released the width is the window's to clamp again:
  // maxFor(800) = 420, and widening brings the dragged 480 back.
  await page.setViewportSize({ width: 800, height: 720 });
  await expectWidth(page, 420, "sidebar width at 800px");
  await page.setViewportSize({ width: 1280, height: 720 });
  await expectWidth(page, 480, "sidebar width back at 1280px");
});

test("the comment rail insets the doc only while the content column has room for it", async ({ page }) => {
  await signIn(page);
  await newDoc(page);
  await expect(page.locator("html")).toHaveAttribute("data-doc-width", "fixed");

  await page.getByTestId("open-comments").click();
  await expect(page.getByTestId("comment-sidebar")).toBeVisible();
  // 1280 - 260 = 1020: exactly the room the inset needs, so with the default
  // sidebar the threshold is still a 1280px window. Polled throughout: the
  // shell transitions its padding over 160ms, so an early read lands mid-way.
  await expect.poll(() => docPaddingRight(page)).toBe("424px"); // 24 + 400
  // One pixel less is the overlay: the threshold is exactly 1020.
  await page.setViewportSize({ width: 1279, height: 720 });
  await expect.poll(() => docPaddingRight(page)).toBe("24px");
  await page.setViewportSize({ width: 1280, height: 720 });
  await expect.poll(() => docPaddingRight(page)).toBe("424px");

  // 1280 - 480 = 800 < 1020: insetting would squeeze the doc to ~400px, so
  // the rail overlays instead, as it already does on narrower screens.
  await focusHandle(page);
  await page.keyboard.press("End");
  await expectWidth(page, 480);
  await expect.poll(() => docPaddingRight(page)).toBe("24px");
  // Still open: the inset went because of the width, not because the rail did.
  await expect(page.getByTestId("comment-sidebar")).toBeVisible();

  // And it comes back as soon as there is room again: 1280 - 200 = 1080.
  await page.keyboard.press("Home");
  await expectWidth(page, 200);
  await expect.poll(() => docPaddingRight(page)).toBe("424px");

  // A zoomed window's 100vw can be fractional, which can leave the column a
  // fraction of a pixel short of 1020. That is still the overlay, not part
  // of the inset. The gate only sees the difference, so a fractional stamp
  // stands in for the zoom here.
  await page.evaluate(() => document.documentElement.style.setProperty("--knot-sidebar-w", "260.8px"));
  await expect.poll(() => docPaddingRight(page)).toBe("24px");
});

test("a keyboard-focused edge stays on top of the open comment rail", async ({ page }) => {
  await page.setViewportSize({ width: 800, height: 720 });
  await signIn(page);
  await newDoc(page);
  await page.getByTestId("open-comments").click();
  const rail = page.getByTestId("comment-sidebar");
  await expect(rail).toBeVisible();

  // maxFor(800) = 420, which puts the edge under the 400px rail.
  await focusHandle(page);
  await page.keyboard.press("End");
  await expectWidth(page, 420);
  const h = (await handle(page).boundingBox())!;
  expect(h.x, "the edge is where the rail is").toBeGreaterThan((await rail.boundingBox())!.x);
  // What is on top at the edge is the separator, focus line and all.
  const atEdge = await page.evaluate(
    ([x, y]) => document.elementFromPoint(x, y)?.getAttribute("data-testid") ?? null,
    [h.x + h.width / 2, h.y + h.height / 2] as const,
  );
  expect(atEdge, "element on top at the focused edge").toBe("sidebar-resize-handle");
});

test("resizing in one tab resizes the sidebar in another, without a reload", async ({ page }) => {
  await signIn(page);
  // A second page in the same context shares localStorage, like a second
  // tab in the same browser profile.
  const other = await page.context().newPage();
  await other.goto("/");
  await expectWidth(other, 260, "other tab's sidebar width");
  // Count the storage events it hears, and plant a marker a reload would wipe.
  await other.evaluate((k) => {
    const w = window as Probe;
    w.__sameDocument = true;
    w.__sidebarEvents = [];
    window.addEventListener("storage", (e) => {
      if (e.key === k) w.__sidebarEvents!.push(e.newValue);
    });
  }, KEY);
  const heard = () => other.evaluate(() => (window as Probe).__sidebarEvents);

  await page.bringToFront();
  const { x, y } = await grab(page);
  await page.mouse.move(x + 100, y, { steps: 10 });
  await expectWidthNear(page, 360);
  // Mid-drag nothing is committed, so the other tab has heard nothing.
  expect(await heard(), "storage events heard mid-drag").toEqual([]);
  await page.mouse.up();

  await expect.poll(() => storedWidth(page)).toEqual(expect.stringMatching(/^\d+$/));
  const saved = (await storedWidth(page))!;
  await expectWidth(other, Number(saved), "other tab's sidebar width");
  await expect(handle(other)).toHaveAttribute("aria-valuenow", saved);
  // One drag, one commit: a single event, carrying the final width.
  expect(await heard(), "storage events heard per drag").toEqual([saved]);
  expect(await other.evaluate(() => (window as Probe).__sameDocument), "other tab reloaded").toBe(true);
});

test("a saved width is on screen from the first frame, with no 260px flash", async ({ page }) => {
  await signIn(page);
  await page.evaluate(([k, v]) => localStorage.setItem(k, v), [KEY, "400"] as const);
  // Record the sidebar's width the moment it enters the DOM. A
  // MutationObserver callback is a microtask: it runs straight after
  // React's commit and before the browser gets a chance to paint, so this
  // is what the first painted frame would show. On this reload path the
  // shell mounts in a synchronous commit, whose passive effects React also
  // flushes before this observer runs. So this pins that some stamp —
  // main.tsx's or AppShell's — lands before first paint, not that AppShell's
  // correction is a layout effect; AppShell.test.tsx pins that.
  await page.addInitScript(() => {
    const w = window as Probe;
    const mo = new MutationObserver(() => {
      const width = document.querySelector("[data-testid='sidebar']")?.getBoundingClientRect().width ?? 0;
      if (width === 0) return; // not in the DOM, or not laid out yet
      w.__firstSidebarWidth = width;
      mo.disconnect();
    });
    mo.observe(document, { childList: true, subtree: true, attributes: true });
  });
  await page.reload();

  await expect
    .poll(() => page.evaluate(() => (window as Probe).__firstSidebarWidth ?? null), {
      message: "sidebar width on its first frame: expected 400px",
    })
    .toBe(400);
});

test("a corrupt or out-of-range saved width is sanitised, never rendered as-is", async ({ page }) => {
  await signIn(page);
  const cases: [saved: string, px: number][] = [
    ["9999", 480], // above the maximum: clamped
    ["50", 200], // below the minimum: clamped
    ["abc", 260], // not a number: the default
    ["Infinity", 260], // not finite: the default
  ];
  for (const [saved, px] of cases) {
    await seedSavedWidth(page, saved);
    await expectWidth(page, px, `sidebar width from a saved "${saved}"`);
  }
});

test("in a forced-colours (Windows contrast) theme the accent line shows in the highlight colour", async ({ page }) => {
  await page.emulateMedia({ forcedColors: "active" });
  await signIn(page);
  await expectWidth(page, 260);
  // Forced colours repaint author colours such as the accent as Canvas, so a
  // line drawn in it would erase the very border it sits on, on hover and
  // for the whole of a drag.
  const { highlight, canvas } = await page.evaluate(() => {
    const probe = document.createElement("div");
    document.body.append(probe);
    probe.style.backgroundColor = "Highlight";
    const highlight = getComputedStyle(probe).backgroundColor;
    probe.style.backgroundColor = "Canvas";
    const canvas = getComputedStyle(probe).backgroundColor;
    probe.remove();
    return { highlight, canvas };
  });
  expect(highlight, "Highlight differs from Canvas in this theme").not.toBe(canvas);

  const line = handle(page).locator("span");
  await handle(page).hover();
  await expect(line).toHaveCSS("opacity", "1");
  await expect(line, "accent line on hover").toHaveCSS("background-color", highlight);

  const { x, y } = await grab(page);
  await page.mouse.move(x + 40, y, { steps: 4 });
  await expect(line, "accent line mid-drag").toHaveCSS("background-color", highlight);
  await page.mouse.up();
});

test.describe("on a phone", () => {
  test.use({ viewport: { width: 375, height: 667 } });

  test("there is no resize handle, and the drawer stays 260px whatever width is saved", async ({ page }) => {
    await signIn(page);
    // A desktop preference must not leak into the drawer.
    await seedSavedWidth(page, "400");
    // The drawer starts open: sidebarOpen defaults to true.
    await expect(page.getByTestId("sidebar-backdrop")).toBeVisible();
    await expectWidth(page, 260, "drawer width");
    await expect(handle(page)).toHaveCount(0);
  });
});
