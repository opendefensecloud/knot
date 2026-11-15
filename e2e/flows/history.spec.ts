import { expect, test, type Locator, type Page } from "@playwright/test";

import { reset } from "../support/reset";

// Each test bootstraps through /setup, and POST /auth/setup answers 410 once
// an owner exists, so reset per test, not once per file.
test.beforeEach(reset);

const V1 = "First version of the doc.";

async function newDoc(page: Page) {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
}

/** Below 640 px the sidebar starts open with its backdrop over the page,
 *  and nothing closes it on navigation; tap its right-hand side, as
 *  mobile.spec.ts does (the sidebar covers the left 260 px). */
async function closeSidebarIfOpen(page: Page) {
  const backdrop = page.getByTestId("sidebar-backdrop");
  if (await backdrop.count()) await backdrop.click({ position: { x: 350, y: 300 } });
  await expect(backdrop).toHaveCount(0);
}

async function openHistory(page: Page) {
  await page.getByTestId("open-history").click();
  await expect(page.getByTestId("history-drawer")).toBeVisible();
}

/**
 * Waits up to 60 s for `entries` to match at least one row. The builder
 * writes a session only after its 10 s debounce and a 2 s tick, and the
 * drawer polls on its own, so the spec only watches the DOM. Once, after
 * 30 s without a row, it reloads the page and reopens the drawer: a reload
 * empties the query cache, so the reopened drawer starts with a fresh first
 * fetch whatever the poll interval is (closing and reopening alone would
 * not, as the cached list is younger than the 30 s staleTime).
 */
async function waitForEntries(page: Page, entries: Locator, narrow = false) {
  const started = Date.now();
  let reopened = false;
  await expect
    .poll(
      async () => {
        const n = await entries.count();
        if (n === 0 && !reopened && Date.now() - started > 30_000) {
          reopened = true;
          await page.reload();
          if (narrow) await closeSidebarIfOpen(page);
          await openHistory(page);
        }
        return n;
      },
      { timeout: 60_000 },
    )
    .toBeGreaterThan(0);
}

/** A colour token as the browser resolves it inside `scope` (an rgb()
 *  string), read from a probe element styled `color: var(<token>)`. */
async function resolvedColor(scope: Locator, token: string): Promise<string> {
  return scope.evaluate((el, name) => {
    const probe = document.createElement("span");
    probe.style.color = `var(${name})`;
    el.appendChild(probe);
    const color = getComputedStyle(probe).color;
    probe.remove();
    return color;
  }, token);
}

async function decoration(mark: Locator): Promise<{ line: string; color: string }> {
  return mark.evaluate((el) => {
    const s = getComputedStyle(el);
    return { line: s.textDecorationLine, color: s.textDecorationColor };
  });
}

test("a session's version shows its text, highlighted, and restores", async ({ page }) => {
  test.setTimeout(120_000);
  await newDoc(page);

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type(V1);
  await page.waitForTimeout(500);

  // The import replaces the body (the page is not empty, so it asks first)
  // and seals the typing session.
  page.once("dialog", (d) => void d.accept());
  await page.getByTestId("doc-import-md-input").setInputFiles({
    name: "replacement.md",
    mimeType: "text/markdown",
    buffer: Buffer.from("# Replacement\n\nOnly this.\n"),
  });
  // Sanity: the editor shows the import, so the last assertion can only
  // pass through the restore.
  await expect(editor).toContainText("Only this.", { timeout: 10_000 });
  await expect(editor).not.toContainText(V1, { timeout: 10_000 });

  await openHistory(page);
  await waitForEntries(page, page.locator("[data-testid^='history-entry-'][data-kind='import']"));
  const session = page.locator("[data-testid^='history-entry-'][data-kind='session']").first();
  await expect(session).toBeVisible({ timeout: 60_000 });
  const importEntry = page.locator("[data-testid^='history-entry-'][data-kind='import']").first();

  // The session's version: its text, marked as added.
  await session.click();
  const doc = page.getByTestId("history-version-doc");
  await expect(doc).toContainText(V1, { timeout: 15_000 });
  const ins = doc.locator("ins.history-insert").first();
  await expect(ins).toBeVisible({ timeout: 15_000 });
  const added = await resolvedColor(doc, "--color-history-added");
  expect(added).toMatch(/^rgb/);
  const insStyle = await decoration(ins);
  expect(insStyle.line).toContain("underline");
  expect(insStyle.color).toBe(added);

  // The import's version: the session's text, struck through.
  await importEntry.click();
  const del = doc.locator("del.history-delete", { hasText: V1 }).first();
  await expect(del).toBeVisible({ timeout: 15_000 });
  const destructive = await resolvedColor(doc, "--color-destructive");
  expect(destructive).toMatch(/^rgb/);
  const delStyle = await decoration(del);
  expect(delStyle.line).toContain("line-through");
  expect(delStyle.color).toBe(destructive);

  // Back to the session, highlighting off.
  await session.click();
  await expect(doc).toContainText(V1, { timeout: 15_000 });
  await page.getByTestId("history-show-changes").click();
  await expect(doc.locator("ins.history-insert")).toHaveCount(0, { timeout: 15_000 });
  await expect(doc).toContainText(V1);

  page.once("dialog", (d) => void d.accept());
  await page.getByTestId("history-restore").click();
  await expect(editor).toContainText(V1, { timeout: 10_000 });
});

test("the drawer stacks on a narrow screen", async ({ page }) => {
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 390, height: 844 });
  await newDoc(page);
  await closeSidebarIfOpen(page);

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type(V1);
  await page.waitForTimeout(500);

  await openHistory(page);
  const timeline = page.getByTestId("history-timeline");
  await expect(timeline).toBeVisible();
  const sessions = page.locator("[data-testid^='history-entry-'][data-kind='session']");
  await waitForEntries(page, sessions, true);

  await sessions.first().click();
  await expect(page.getByTestId("history-version")).toBeVisible({ timeout: 15_000 });
  await expect(timeline).toBeHidden();

  await page.getByTestId("history-back").click();
  await expect(timeline).toBeVisible();
});
