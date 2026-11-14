import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

const EMAIL = "o@e.com";
const PASSWORD = "owner-hunter22";

// The owner is created against the API rather than through /setup, because
// /setup renders its form whether or not an owner exists — so a second test
// in this file cannot tell the two states apart from the page alone.
test.beforeAll(async () => {
  reset();
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, display_name: "O", password: PASSWORD }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});
test.use({ viewport: { width: 375, height: 667 } });

async function signIn(page: Page) {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
}

test("mobile: drawer opens/closes, palette goes full-screen", async ({ page }) => {
  await signIn(page);

  // Initial state: sidebarOpen=true (Zustand default), so backdrop is showing.
  const backdrop = page.getByTestId("sidebar-backdrop");
  await expect(backdrop).toBeVisible();
  // Close drawer by tapping the backdrop's right-hand region (the sidebar
  // sits over the left 260 px and intercepts clicks there).
  await backdrop.click({ position: { x: 350, y: 300 } });
  await expect(backdrop).toHaveCount(0);

  // Hamburger toggle now visible.
  const toggle = page.getByTestId("menu-toggle");
  await expect(toggle).toBeVisible();

  // Open drawer via toggle.
  await toggle.click();
  await expect(backdrop).toBeVisible();
  await expect(page.getByTestId("sidebar")).toBeVisible();

  // Close again to free input focus before opening palette.
  await backdrop.click({ position: { x: 350, y: 300 } });
  await expect(backdrop).toHaveCount(0);

  // Open palette — should cover viewport width.
  await page.keyboard.press("Control+k");
  const dialog = page.getByTestId("cmdk");
  await expect(dialog).toBeVisible();
  const box = await dialog.boundingBox();
  expect(box?.width).toBeGreaterThan(370);
  expect(box?.height).toBeGreaterThan(660);
});

test("mobile: the document header fits the viewport", async ({ page }) => {
  await signIn(page);
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("doc-page")).toBeVisible();

  // The header action row carries eleven controls for the page's owner, the
  // audit log link among them. Without wrapping it is far wider than the
  // 327px content box, pushing the last buttons off-screen and giving <main>
  // a horizontal scrollbar.
  const overflow = await page.evaluate(() => {
    const m = document.querySelector("main")!;
    return m.scrollWidth - m.clientWidth;
  });
  expect(overflow).toBeLessThanOrEqual(1);

  // Every control must be reachable, not merely non-overflowing.
  for (const id of [
    "toggle-edit-mode",
    "open-audit",
    "toggle-markdown",
    "doc-export",
    "toggle-template",
    "open-comments",
  ]) {
    await expect(page.getByTestId(id)).toBeInViewport();
  }

  // The wrapped row puts the export trigger near the left edge; its menu
  // still opens fully on screen. The drawer, still open over the left of
  // the page, goes first.
  const backdrop = page.getByTestId("sidebar-backdrop");
  if ((await backdrop.count()) > 0) await backdrop.click({ position: { x: 350, y: 300 } });
  await expect(backdrop).toHaveCount(0);
  await page.getByTestId("doc-export").click();
  for (const id of ["export-docx", "export-odt", "export-md", "export-md-subtree"]) {
    await expect(page.getByTestId(id)).toBeInViewport({ ratio: 1 });
  }
  await page.keyboard.press("Escape");
  await expect(page.getByRole("menu")).toHaveCount(0);
});

test("mobile: the audit log fits the viewport", async ({ page }) => {
  await signIn(page);
  const csrf = (await page.context().cookies()).find((cookie) => cookie.name === "csrf");
  if (!csrf) throw new Error("csrf cookie not found");
  const created = await page.request.post("/api/docs", {
    headers: { "X-CSRF-Token": csrf.value },
    data: { title: "A page with a rather long title to see how the audit log wraps it" },
  });
  expect(created.status()).toBe(201);
  const docId = ((await created.json()) as { id: string }).id;
  // One recorded export, so the stats, the exporters list and an event row
  // all render.
  const exported = await page.request.get(`/api/docs/${docId}/export`);
  expect(exported.status()).toBe(200);

  await page.goto(`/doc/${docId}/audit`);
  await expect(page.getByTestId("doc-audit-page")).toBeVisible({ timeout: 10_000 });
  await expect(page.getByTestId("audit-exporters")).toBeVisible({ timeout: 10_000 });
  await expect(page.locator("[data-testid='audit-event'][data-action='doc.export']")).toHaveCount(1);
  await expect(page.getByTestId("audit-scope-note")).toBeVisible();

  const overflow = await page.evaluate(() => {
    const m = document.querySelector("main")!;
    return Math.max(
      m.scrollWidth - m.clientWidth,
      document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
  });
  expect(overflow).toBeLessThanOrEqual(1);
  // The tiles go two to a row, so both of the first row sit on screen.
  for (const id of ["audit-stat-exports", "audit-stat-people"]) {
    await expect(page.getByTestId(id)).toBeInViewport({ ratio: 1 });
  }
});
