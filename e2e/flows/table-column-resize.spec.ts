import { expect, test, type Locator, type Page } from "@playwright/test";

import { reset } from "../support/reset";

// Reset once: the owner is created in the beforeAll below, and a per-test
// reset would delete it out from under the later tests.
test.beforeAll(reset);

// This flow needs the production default — existing docs open in view mode,
// so the editor is created read-only — which the config's edit-mode-by-default
// flag would hide. That default is exactly what kept the old resize handles
// from ever appearing.
test.use({ storageState: { cookies: [], origins: [] } });

const EMAIL = "owner@tables.test";
const PASSWORD = "hunter22!tables";

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

async function waitConnected(page: Page) {
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
}

async function newDoc(page: Page) {
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await waitConnected(page);
}

async function width(locator: Locator) {
  return (await locator.boundingBox())!.width;
}

test("a column border dragged after switching to Edit resizes the column and survives a reload", async ({
  page,
}) => {
  await signIn(page);
  await newDoc(page); // a new doc opens in edit mode

  const prose = page.locator("[data-testid='editor-host'] .ProseMirror");
  await prose.click();
  await page.getByTestId("toolbar-table").click();
  await expect(prose.locator("table")).toBeVisible();

  // Leave edit mode and reload: the editor is now created read-only, the way
  // every existing doc opens.
  const toggle = page.getByTestId("toggle-edit-mode");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-pressed", "false");
  await page.waitForTimeout(800);
  await page.reload();
  await waitConnected(page);

  const border = prose.locator('.knot-col-handle[data-border="0"]');
  const firstHeader = prose.locator("tr").first().locator("th").first();
  await expect(firstHeader).toBeVisible();
  await expect(border).toBeHidden();

  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-pressed", "true");
  await expect(border).toBeVisible();

  const before = await width(firstHeader);
  const handle = (await border.boundingBox())!;
  const x = handle.x + handle.width / 2;
  const y = handle.y + handle.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + 120, y, { steps: 5 });
  await page.mouse.up();

  await expect.poll(() => width(firstHeader)).toBeGreaterThan(before + 100);

  // The table still fills its wrapper: the other columns gave up the width.
  const overflow = await prose.locator(".tableWrapper").evaluate((el) => el.scrollWidth - el.clientWidth);
  expect(overflow).toBeLessThanOrEqual(1);

  await page.waitForTimeout(800);
  await page.reload();
  await waitConnected(page);
  await expect.poll(() => width(firstHeader)).toBeGreaterThan(before + 100);
});
