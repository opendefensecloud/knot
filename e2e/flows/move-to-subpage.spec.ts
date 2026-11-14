import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

/**
 * Move to subpage (issue #92): a commented paragraph moves into a new child
 * page, and its comment is highlighted there, on the same words.
 */

test.beforeAll(reset);

const EMAIL = "owner@move.test";
const PASSWORD = "hunter22!move";

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

async function connected(page: Page) {
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 15_000,
  });
}

/** Select the last `n` characters before the caret. */
async function selectBack(page: Page, n: number) {
  for (let i = 0; i < n; i += 1) await page.keyboard.press("Shift+ArrowLeft");
}

test("a commented paragraph moves to a subpage with its comment", async ({ page }) => {
  await signIn(page);
  await page.getByTestId("new-doc").click();
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await connected(page);
  const sourceUrl = page.url();

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("stays here");
  await page.keyboard.press("Enter");
  await page.keyboard.type("moves away");

  // Comment on "away".
  await selectBack(page, "away".length);
  await page.getByTestId("add-comment-float").click();
  await page.getByTestId("comment-composer-input-new").fill("is this right?");
  await page.getByTestId("comment-composer-submit-new").click();
  const highlight = page.locator("[data-testid='editor-host'] .comment-highlight");
  await expect(highlight).toHaveText("away");

  // Select within the second paragraph and move it.
  await editor.locator("p").nth(1).click();
  await page.keyboard.press("End");
  await selectBack(page, "away".length);
  page.once("dialog", (dialog) => void dialog.accept("Moved part"));
  await page.getByTestId("move-to-subpage-float").click();

  // The source keeps the first paragraph and a link where the second was.
  const link = editor.locator("a", { hasText: "Moved part" });
  await expect(link).toBeVisible({ timeout: 10_000 });
  await expect(editor).toContainText("stays here");
  await expect(editor).not.toContainText("moves away");
  await expect(highlight).toHaveCount(0);

  // The subpage holds the paragraph, and the comment is highlighted there.
  await link.click();
  await page.waitForURL((url) => url.href !== sourceUrl);
  await connected(page);
  await expect(editor).toContainText("moves away");
  await expect(highlight).toHaveText("away");
});
