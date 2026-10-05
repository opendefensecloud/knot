import { expect, test, type Browser, type Page } from "@playwright/test";

import { docText } from "../support/docText";
import { reset } from "../support/reset";

test.beforeAll(reset);

const editorOf = (page: Page) => page.locator("[data-testid='editor-host'] .ProseMirror");

async function signIn(browser: Browser, email: string, password: string): Promise<Page> {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  await page.goto("/login");
  await page.getByTestId("login-email").fill(email);
  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/, { timeout: 5_000 });
  return page;
}

async function invite(page: Page, email: string, role: "editor" | "viewer", password: string) {
  await page.goto("/members");
  await page.getByTestId("invite-email").fill(email);
  await page.getByTestId("invite-role").selectOption(role);
  await page.getByTestId("invite-password").fill(password);
  await page.getByTestId("invite-submit").click();
  await expect(page.getByTestId("members-table").locator("tr", { hasText: email })).toHaveCount(1, {
    timeout: 5_000,
  });
}

/** Open the contributors popover (which refetches) and return its text. */
async function contributorsText(page: Page): Promise<string> {
  const button = page.getByTestId("doc-contributors-button");
  const popover = page.getByTestId("doc-contributors-popover");
  if (await popover.isVisible()) await button.click();
  await button.click();
  await expect(popover).toBeVisible();
  // Opening refetches; give the fresh list a moment to replace the cached one.
  await page.waitForLoadState("networkidle");
  return (await popover.innerText()).toLowerCase();
}

test("byline names the creator and lists only the people who edited the content", async ({ browser }) => {
  // Alice sets up the workspace and writes the first lines of a page.
  const aliceCtx = await browser.newContext();
  const alice = await aliceCtx.newPage();
  await alice.goto("/setup");
  await alice.getByTestId("setup-email").fill("alice@example.com");
  await alice.getByTestId("setup-display-name").fill("Alice");
  await alice.getByTestId("setup-password").fill("alice-hunter22");
  await alice.getByTestId("setup-submit").click();
  await alice.getByTestId("new-doc").click();
  await alice.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await alice.getByTestId("new-doc-blank").click();
  await alice.waitForURL(/\/doc\/.+/);
  const docUrl = alice.url();
  await expect(alice.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  await expect(alice.getByTestId("doc-byline-creator")).toContainText("Created by Alice");
  // Nobody has edited yet, so there is nothing to list.
  await expect(alice.getByTestId("doc-contributors-button")).toHaveCount(0);

  await editorOf(alice).click();
  await alice.keyboard.type("Written by Alice. ");

  // Live edits are persisted in 250 ms batches; the byline is fetched on load.
  await expect(async () => {
    await alice.reload();
    await expect(alice.getByTestId("doc-contributors-button")).toContainText("1 contributor", { timeout: 2_000 });
  }).toPass({ timeout: 15_000 });
  expect(await contributorsText(alice)).toContain("alice");

  await invite(alice, "bob@example.com", "editor", "bob-hunter22");
  await invite(alice, "vera@example.com", "viewer", "vera-hunter22");
  await alice.goto(docUrl);

  // Bob, an editor, opens the page and only watches.
  const bob = await signIn(browser, "bob@example.com", "bob-hunter22");
  await bob.goto(docUrl);
  await expect(bob.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });
  await expect.poll(() => editorOf(bob).evaluate(docText), { timeout: 8_000 }).toMatch(/Written by Alice\./);

  // While Bob watches, Alice types — including a URL, so autolink runs (a bare
  // www. domain: typing "//" opens knot's date picker). Watching must never
  // credit Bob: not the handshake his editor answered on connect, and not any
  // edit his editor might send in reaction to hers. (Alice's own update
  // already carries the link mark, so this path does not exercise the
  // KnotLink remote-transaction guard; autolink.test.ts covers that.)
  await expect(alice.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });
  await editorOf(alice).click();
  await alice.keyboard.press("End");
  await alice.keyboard.type("See www.example.com and more.");
  await expect(editorOf(alice).locator("a[href*='example.com']")).toHaveCount(1, { timeout: 5_000 });
  await expect(editorOf(bob).locator("a[href*='example.com']")).toHaveCount(1, { timeout: 8_000 });
  // A phantom edit from Bob's editor would be sent immediately and persisted
  // within one 250 ms writer batch. Wait well past that before asserting it
  // never happened.
  await bob.waitForTimeout(2_000);

  const afterWatching = await contributorsText(alice);
  expect(afterWatching).toContain("alice");
  expect(afterWatching, "Bob only had the page open; he must not be listed").not.toContain("bob");
  await expect(alice.getByTestId("doc-contributors-button")).toContainText("1 contributor");

  // Now Bob edits, and becomes a contributor.
  await editorOf(bob).click();
  await bob.keyboard.press("End");
  await bob.keyboard.type(" Bob was here.");
  await expect.poll(() => editorOf(alice).evaluate(docText), { timeout: 8_000 }).toMatch(/Bob was here\./);
  await expect.poll(() => contributorsText(alice), { timeout: 10_000 }).toContain("bob");
  await expect(alice.getByTestId("doc-contributors-button")).toContainText("2 contributors");

  // A viewer sees the byline too, and is not listed for having looked.
  const vera = await signIn(browser, "vera@example.com", "vera-hunter22");
  await vera.goto(docUrl);
  await expect(vera.getByTestId("doc-byline-creator")).toContainText("Created by Alice");
  await expect(vera.getByTestId("doc-contributors-button")).toContainText("2 contributors");
  const seenByViewer = await contributorsText(vera);
  expect(seenByViewer).toContain("alice");
  expect(seenByViewer).toContain("bob");
  expect(seenByViewer).not.toContain("vera");

  await aliceCtx.close();
  await bob.context().close();
  await vera.context().close();
});
