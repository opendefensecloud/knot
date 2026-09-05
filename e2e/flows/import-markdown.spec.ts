import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

// Each test bootstraps through /setup, and POST /auth/setup returns 410 once a
// user exists — so reset per test, not once per file.
test.beforeEach(reset);

async function newDoc(page: Page) {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("i@example.com");
  await page.getByTestId("setup-display-name").fill("I");
  await page.getByTestId("setup-password").fill("hunter22!hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
}

test("import a .md file into an empty page, then replace it", async ({ page }) => {
  await newDoc(page);
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");

  await page.getByTestId("doc-import-md-input").setInputFiles({
    name: "notes.md",
    mimeType: "text/markdown",
    buffer: Buffer.from("# Imported Heading\n\n- alpha\n- beta\n"),
  });

  await expect(editor.locator("h1")).toHaveText("Imported Heading", { timeout: 10_000 });
  await expect(editor.locator("li")).toHaveCount(2);
  // Plain bullets must not pick up a checkbox.
  await expect(editor.locator("li[data-checked]")).toHaveCount(0);

  // Second import into the now-non-empty page: accept the confirm and check
  // the old body is gone rather than appended to.
  page.once("dialog", (d) => void d.accept());
  await page.getByTestId("doc-import-md-input").setInputFiles({
    name: "replacement.md",
    mimeType: "text/markdown",
    buffer: Buffer.from("# Replacement Heading\n\nOnly this.\n"),
  });

  await expect(editor.locator("h1")).toHaveText("Replacement Heading", { timeout: 10_000 });
  await expect(editor).not.toContainText("Imported Heading");
  await expect(editor).not.toContainText("alpha");
});

test("paste Markdown source into the editor", async ({ page }) => {
  await newDoc(page);
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();

  await page.evaluate(() => {
    const dt = new DataTransfer();
    dt.setData("text/plain", "## Pasted Heading\n\n- one\n- two\n");
    const pm = document.querySelector("[data-testid='editor-host'] .ProseMirror")!;
    pm.dispatchEvent(
      new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: dt }),
    );
  });

  await expect(editor.locator("h2")).toHaveText("Pasted Heading", { timeout: 10_000 });
  await expect(editor.locator("li")).toHaveCount(2);
});

// Checklists and tables are first-class in knot, and both arrive through the
// server's Markdown parser, which stores `checked` as a Yjs string rather than
// the boolean the editor's own input rules produce. Importing used to render
// those items as plain bullets.
test("an imported checklist keeps its checkboxes", async ({ page }) => {
  await newDoc(page);
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");

  await page.getByTestId("doc-import-md-input").setInputFiles({
    name: "plan.md",
    mimeType: "text/markdown",
    buffer: Buffer.from(
      "# Release Plan\n\n" +
        "| env | owner |\n| --- | --- |\n| prod | ops |\n\n" +
        "- [ ] cut the tag\n- [x] write the notes\n",
    ),
  });

  await expect(editor.locator("h1")).toHaveText("Release Plan", { timeout: 10_000 });
  await expect(editor.locator("td").first()).toHaveText("prod");
  await expect(editor.locator("li[data-checked='false']")).toHaveText("cut the tag");
  await expect(editor.locator("li[data-checked='true']")).toHaveText("write the notes");
});
