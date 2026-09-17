import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

const EMAIL = "owner@header.test";
const PASSWORD = "hunter22!header";

// /setup renders its form whether or not an owner exists, so the owner is
// created against the API and every test signs in normally.
test.beforeAll(async () => {
  reset();
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

async function newDoc(page: Page) {
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
}

/** How many pixels of the title the input cannot show. An <input> never
 *  wraps and clips its own overflow, so this is the only signal that the
 *  title has been cut off — it is not something a visibility check sees. */
async function titleClippedBy(page: Page) {
  return page.getByTestId("doc-title").evaluate((el) => el.scrollWidth - el.clientWidth);
}

// Issue #29: on the desktop column the title shared its line with the
// action row, which left it 208px — room for about 15 characters at 30px
// bold before the input cut the rest off behind the tool icons.
test("desktop: a realistic title is shown in full, not cut off by the tool icons", async ({ page }) => {
  await signIn(page);
  await newDoc(page);

  const title = "Quarterly Infrastructure Review 2026";
  const input = page.getByTestId("doc-title");
  await input.fill(title);
  await input.blur();
  await expect(input).toHaveValue(title);

  expect(await titleClippedBy(page)).toBe(0);
});
