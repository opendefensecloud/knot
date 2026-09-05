import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);

test("toolbar toggles bold + heading", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute(
    "data-status",
    "connected",
    { timeout: 10_000 },
  );

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("hello world");
  await page.keyboard.press("ControlOrMeta+a");

  await page.getByTestId("toolbar-bold").click();
  await expect(editor.locator("strong")).toContainText("hello world");

  await page.getByTestId("toolbar-h1").click();
  await expect(editor.locator("h1")).toBeVisible();
});

test("toolbar buttons reflect what the caret is inside", async ({ page }) => {
  // The buttons above assert on the document; nothing asserts that the
  // toolbar itself stays in sync with the selection. `active` comes from
  // editor.isActive(...) read during render, so it only updates if something
  // re-renders the toolbar on every transaction — a default that a Tiptap
  // React version bump can flip without any type or build error.
  await page.goto("/login");
  await page.getByTestId("login-email").fill("o@e.com");
  await page.getByTestId("login-password").fill("owner-hunter22");
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  const bold = page.getByTestId("toolbar-bold");
  const h1 = page.getByTestId("toolbar-h1");

  await editor.click();
  await expect(bold).toHaveAttribute("aria-pressed", "false");
  await expect(h1).toHaveAttribute("aria-pressed", "false");

  // Inside a heading, the heading button reports pressed…
  await h1.click();
  await page.keyboard.type("a heading");
  await expect(h1).toHaveAttribute("aria-pressed", "true");

  // …and stops reporting it once the caret leaves, driven by the selection
  // alone rather than by a click on the button.
  await page.keyboard.press("Enter");
  await page.keyboard.type("body text");
  await expect(h1).toHaveAttribute("aria-pressed", "false");

  // Same for a mark: select the body text and toggle it.
  for (let i = 0; i < "body text".length; i += 1) {
    await page.keyboard.press("Shift+ArrowLeft");
  }
  await bold.click();
  await expect(bold).toHaveAttribute("aria-pressed", "true");

  // The table row/column controls are gated on isActive("table") and no spec
  // touches them. Absent in a paragraph, present once the caret is in a table
  // — which only holds if the toolbar re-reads isActive as the caret moves.
  await expect(page.getByTestId("toolbar-table-add-row")).toHaveCount(0);
  await page.getByTestId("toolbar-table").click();
  await expect(page.getByTestId("toolbar-table-add-row")).toBeVisible();
});
