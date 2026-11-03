import { expect, test, type Browser, type Page } from "@playwright/test";

import { docText } from "../support/docText";
import { reset } from "../support/reset";

/**
 * Acceptance proof for change suggestions: an editor proposes a change in
 * the sidebar, the page stays as it is, and an owner accepts or rejects it.
 * Each test starts from an empty workspace.
 */
test.beforeEach(reset);
// Fail at the step that stalls instead of at the test timeout.
test.use({ actionTimeout: 15_000 });

const OWNER_EMAIL = "owner@sg.test";
const OWNER_PASSWORD = "owner-hunter22";
const SENTENCE = "alpha bravo charlie";

const editorOf = (page: Page) => page.locator("[data-testid='editor-host'] .ProseMirror");

async function connected(page: Page) {
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 15_000,
  });
}

async function setupOwner(browser: Browser): Promise<Page> {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill(OWNER_EMAIL);
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill(OWNER_PASSWORD);
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
  return page;
}

async function signIn(browser: Browser, email: string, password: string): Promise<Page> {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  await page.goto("/login");
  await page.getByTestId("login-email").fill(email);
  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/, { timeout: 10_000 });
  return page;
}

async function invite(
  page: Page,
  email: string,
  role: "owner" | "editor" | "viewer",
  password: string,
) {
  await page.goto("/members");
  await page.getByTestId("invite-email").fill(email);
  await page.getByTestId("invite-role").selectOption(role);
  await page.getByTestId("invite-password").fill(password);
  await page.getByTestId("invite-submit").click();
  await expect(page.getByTestId("members-table").locator("tr", { hasText: email })).toHaveCount(1, {
    timeout: 5_000,
  });
}

async function newDoc(page: Page): Promise<string> {
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await connected(page);
  return page.url().match(/\/doc\/([^/?#]+)/)?.[1] ?? "";
}

async function openDoc(page: Page, docId: string) {
  await page.goto(`/doc/${docId}`);
  await connected(page);
}

/** Open the sidebar on its Suggestions tab. */
async function openSuggestions(page: Page) {
  const tab = page.getByTestId("sidebar-tab-suggestions");
  if (!(await tab.isVisible())) await page.getByTestId("open-comments").click();
  await tab.click();
  await expect(tab).toHaveAttribute("aria-pressed", "true");
}

/** The editor's own selection. Synthetic keypresses can run ahead of it:
 *  ProseMirror reads the DOM selection back asynchronously. */
function pmSelection(page: Page): Promise<{ from: number; to: number }> {
  return editorOf(page).evaluate((el) => {
    const { from, to } = (el as unknown as { editor: { state: { selection: { from: number; to: number } } } })
      .editor.state.selection;
    return { from, to };
  });
}

/** Put the caret at the end of the first paragraph without Home/End chords,
 *  and wait until the editor has caught up with it. */
async function caretToEnd(page: Page) {
  await editorOf(page).click();
  for (let i = 0; i < 40; i += 1) await page.keyboard.press("ArrowRight");
  const domEnd = await editorOf(page).evaluate((el) => el.querySelector("p")?.textContent?.length ?? 0);
  // Paragraph content starts at position 1.
  await expect.poll(() => pmSelection(page), { timeout: 5_000 }).toEqual({ from: domEnd + 1, to: domEnd + 1 });
}

/** Select `word` (its first occurrence in the page) with a mouse double-click. */
async function dblclickWord(page: Page, word: string) {
  const box = await editorOf(page).evaluate((el, w) => {
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    let n: Node | null;
    while ((n = walker.nextNode())) {
      const i = (n.textContent ?? "").indexOf(w);
      if (i < 0) continue;
      const range = document.createRange();
      range.setStart(n, i);
      range.setEnd(n, i + w.length);
      const r = range.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    }
    return null;
  }, word);
  if (!box) throw new Error(`"${word}" is not in the page`);
  await page.mouse.dblclick(box.x, box.y);
}

/** Select the last `n` characters of the paragraph the caret ends. */
async function selectBack(page: Page, n: number) {
  for (let i = 0; i < n; i += 1) await page.keyboard.press("Shift+ArrowLeft");
}

async function waitForText(page: Page, text: string) {
  await expect.poll(() => editorOf(page).evaluate(docText), { timeout: 10_000 }).toBe(text);
}

/** Suggest `replacement` for the current selection and return the new
 *  suggestion's id, read from the card that appears in the author's sidebar. */
async function suggestReplacement(page: Page, replacement: string, before: string[] = []): Promise<string> {
  await page.getByTestId("suggest-change-float").click();
  await expect(page.getByTestId("suggestion-composer")).toBeVisible();
  await page.getByTestId("suggestion-composer-replace").fill(replacement);
  await page.getByTestId("suggestion-composer-submit").click();
  await expect(page.getByTestId("suggestion-composer")).toHaveCount(0, { timeout: 10_000 });
  return newCardId(page, before);
}

async function cardIds(page: Page): Promise<string[]> {
  const ids = await page
    .locator("[data-testid^='suggestion-card-']")
    .evaluateAll((els) => els.map((e) => e.getAttribute("data-testid") ?? ""));
  return ids.map((id) => id.replace("suggestion-card-", ""));
}

async function newCardId(page: Page, before: string[]): Promise<string> {
  let found = "";
  await expect
    .poll(
      async () => {
        found = (await cardIds(page)).find((id) => !before.includes(id)) ?? "";
        return found;
      },
      { timeout: 10_000 },
    )
    .not.toBe("");
  return found;
}

/** Owner + editor on one doc holding SENTENCE; the editor has suggested
 *  "delta" for "charlie". Both pages are on the doc. */
async function ownerEditorWithSuggestion(browser: Browser) {
  const owner = await setupOwner(browser);
  await invite(owner, "ed@sg.test", "editor", "ed-hunter22");
  await owner.goto("/");
  const docId = await newDoc(owner);
  await editorOf(owner).click();
  await owner.keyboard.type(SENTENCE);

  const editor = await signIn(browser, "ed@sg.test", "ed-hunter22");
  await openDoc(editor, docId);
  await waitForText(editor, SENTENCE);
  await caretToEnd(editor);
  await selectBack(editor, "charlie".length);
  const sid = await suggestReplacement(editor, "delta");
  return { owner, editor, docId, sid };
}

async function closeAll(...pages: Page[]) {
  for (const p of pages) await p.context().close();
}

test("editor suggests, owner sees it live and accepts", async ({ browser }) => {
  test.setTimeout(120_000);
  const owner = await setupOwner(browser);
  let editor: Page | undefined;
  try {
    await invite(owner, "ed@sg.test", "editor", "ed-hunter22");
    await owner.goto("/");
    const docId = await newDoc(owner);
    await editorOf(owner).click();
    await owner.keyboard.type(SENTENCE);
    await openSuggestions(owner);

    editor = await signIn(browser, "ed@sg.test", "ed-hunter22");
    await openDoc(editor, docId);
    await waitForText(editor, SENTENCE);
    await caretToEnd(editor);
    await selectBack(editor, "charlie".length);
    const sid = await suggestReplacement(editor, "delta");

    // The page itself is unchanged until an owner accepts.
    await waitForText(editor, SENTENCE);

    const card = owner.getByTestId(`suggestion-card-${sid}`);
    await expect(card).toBeVisible({ timeout: 10_000 });
    await expect(card).toHaveAttribute("data-state", "same");
    await expect(owner.locator(`.suggestion-highlight[data-suggestion-id='${sid}']`)).toHaveText("charlie");

    await owner.getByTestId(`suggestion-accept-${sid}`).click();
    await waitForText(owner, "alpha bravo delta");
    await waitForText(editor, "alpha bravo delta");

    // Resolved: gone from the open list, listed under Resolved as accepted.
    await expect(card).toHaveCount(0, { timeout: 10_000 });
    await owner.getByTestId("suggestion-show-resolved").click();
    await expect(owner.getByTestId(`suggestion-card-${sid}`)).toHaveAttribute("data-state", "accepted");
  } finally {
    await closeAll(owner, ...(editor ? [editor] : []));
  }
});

test('direct edit makes the card "changed", accept replaces current text', async ({ browser }) => {
  test.setTimeout(120_000);
  const { owner, editor, sid } = await ownerEditorWithSuggestion(browser);
  try {
    // Edit inside the word, keeping its last character alive: "charXlie".
    await caretToEnd(editor);
    for (let i = 0; i < 3; i += 1) await editor.keyboard.press("ArrowLeft");
    await editor.keyboard.type("X");
    await waitForText(owner, "alpha bravo charXlie");

    await openSuggestions(owner);
    const card = owner.getByTestId(`suggestion-card-${sid}`);
    await expect(card).toHaveAttribute("data-state", "changed", { timeout: 10_000 });
    await expect(card).toContainText("Changed since it was suggested");
    await expect(card).toContainText("When suggested");
    await expect(card).toContainText("charlie");
    await expect(card).toContainText("Now");
    await expect(card).toContainText("charXlie");

    await owner.getByTestId(`suggestion-accept-${sid}`).click();
    await waitForText(owner, "alpha bravo delta");
    await waitForText(editor, "alpha bravo delta");
  } finally {
    await closeAll(owner, editor);
  }
});

test("text changes while owner looks → 409 and refresh", async ({ browser }) => {
  test.setTimeout(120_000);
  const { owner, editor, docId, sid } = await ownerEditorWithSuggestion(browser);
  try {
    await openSuggestions(owner);
    const card = owner.getByTestId(`suggestion-card-${sid}`);
    await expect(card).toHaveAttribute("data-state", "same", { timeout: 10_000 });

    // Hold the owner's accept until the editor's edit has reached the server.
    let release: () => void = () => {};
    const gate = new Promise<void>((r) => {
      release = r;
    });
    await owner.route("**/api/docs/*/suggestions/*/accept", async (route) => {
      await gate;
      await route.continue();
    });
    const firstResponse = owner.waitForResponse((r) => r.url().endsWith(`/suggestions/${sid}/accept`));
    await owner.getByTestId(`suggestion-accept-${sid}`).click();

    await caretToEnd(editor);
    for (let i = 0; i < 3; i += 1) await editor.keyboard.press("ArrowLeft");
    await editor.keyboard.type("X");
    await expect
      .poll(async () => (await editor.request.get(`/api/docs/${docId}/markdown`)).text(), {
        timeout: 15_000,
      })
      .toContain("charXlie");

    release();
    const res = await firstResponse;
    expect(res.status()).toBe(409);
    const body = (await res.json()) as { error?: { code?: string } };
    expect(body.error?.code).toBe("suggestion.changed");
    await expect(owner.getByTestId(`suggestion-note-${sid}`)).toContainText(
      "The text changed while you were looking. Check it and accept again.",
    );
    await owner.unroute("**/api/docs/*/suggestions/*/accept");

    await expect(card).toHaveAttribute("data-state", "changed", { timeout: 10_000 });
    const second = owner.waitForResponse((r) => r.url().endsWith(`/suggestions/${sid}/accept`));
    await owner.getByTestId(`suggestion-accept-${sid}`).click();
    expect((await second).status()).toBe(200);
    await waitForText(editor, "alpha bravo delta");
  } finally {
    await closeAll(owner, editor);
  }
});

test("deleted target disables Accept", async ({ browser }) => {
  test.setTimeout(120_000);
  const { owner, editor, sid } = await ownerEditorWithSuggestion(browser);
  try {
    // One character at a time: a Backspace sent right after a keyboard
    // selection can reach ProseMirror before the selection does.
    await caretToEnd(editor);
    for (let i = 0; i < SENTENCE.length; i += 1) await editor.keyboard.press("Backspace");
    await waitForText(owner, "");

    await openSuggestions(owner);
    await expect(owner.getByTestId(`suggestion-card-${sid}`)).toHaveAttribute("data-state", "removed", {
      timeout: 10_000,
    });
    await expect(owner.getByTestId(`suggestion-accept-${sid}`)).toBeDisabled();
  } finally {
    await closeAll(owner, editor);
  }
});

test("reject and withdraw", async ({ browser }) => {
  test.setTimeout(120_000);
  const { owner, editor, sid } = await ownerEditorWithSuggestion(browser);
  try {
    await openSuggestions(owner);
    await expect(owner.getByTestId(`suggestion-card-${sid}`)).toBeVisible({ timeout: 10_000 });
    await owner.getByTestId(`suggestion-reject-${sid}`).click();
    await expect(owner.getByTestId(`suggestion-card-${sid}`)).toHaveCount(0, { timeout: 10_000 });

    // The editor proposes another change and takes it back. By mouse: a
    // keyboard selection made right after the first suggestion can be left
    // unread by ProseMirror, which keeps "charlie" as its selection.
    await dblclickWord(editor, "bravo");
    const before = await cardIds(editor);
    const second = await suggestReplacement(editor, "echo", before);
    await expect(editor.getByTestId(`suggestion-withdraw-${second}`)).toBeVisible();
    await editor.getByTestId(`suggestion-withdraw-${second}`).click();
    await expect(editor.getByTestId(`suggestion-card-${second}`)).toHaveCount(0, { timeout: 10_000 });

    await editor.getByTestId("suggestion-show-resolved").click();
    await expect(editor.getByTestId(`suggestion-card-${sid}`)).toHaveAttribute("data-state", "rejected");
    await expect(editor.getByTestId(`suggestion-card-${second}`)).toHaveAttribute("data-state", "withdrawn");

    // Neither touched the page.
    await waitForText(owner, SENTENCE);
  } finally {
    await closeAll(owner, editor);
  }
});

test("viewer sees suggestions read-only", async ({ browser }) => {
  test.setTimeout(120_000);
  const { owner, editor, docId, sid } = await ownerEditorWithSuggestion(browser);
  let viewer: Page | undefined;
  try {
    await invite(owner, "vi@sg.test", "viewer", "vi-hunter22");
    viewer = await signIn(browser, "vi@sg.test", "vi-hunter22");
    await openDoc(viewer, docId);
    await waitForText(viewer, SENTENCE);

    await editorOf(viewer).getByText(SENTENCE).dblclick({ position: { x: 4, y: 6 } });
    await expect(viewer.getByTestId("suggest-change-float")).toHaveCount(0);

    await openSuggestions(viewer);
    await expect(viewer.getByTestId(`suggestion-card-${sid}`)).toBeVisible({ timeout: 10_000 });
    await expect(viewer.getByTestId(`suggestion-waiting-${sid}`)).toBeVisible();
    await expect(viewer.getByTestId(`suggestion-accept-${sid}`)).toHaveCount(0);
    await expect(viewer.getByTestId(`suggestion-reject-${sid}`)).toHaveCount(0);
    await expect(viewer.getByTestId(`suggestion-withdraw-${sid}`)).toHaveCount(0);
  } finally {
    await closeAll(owner, editor, ...(viewer ? [viewer] : []));
  }
});

test("view mode: replace works, insertion button absent", async ({ browser }) => {
  test.setTimeout(120_000);
  const owner = await setupOwner(browser);
  try {
    await newDoc(owner);
    await editorOf(owner).click();
    await owner.keyboard.type(SENTENCE);

    const toggle = owner.getByTestId("toggle-edit-mode");
    await toggle.click();
    await expect(toggle).toHaveAttribute("aria-pressed", "false");
    await expect(owner.getByTestId("toolbar-suggest")).toHaveCount(0);

    await editorOf(owner).getByText(SENTENCE).dblclick({ position: { x: 4, y: 6 } });
    const sid = await suggestReplacement(owner, "omega");
    await expect(owner.getByTestId(`suggestion-card-${sid}`)).toHaveAttribute("data-state", "same", {
      timeout: 10_000,
    });
    await expect(owner.locator(`.suggestion-highlight[data-suggestion-id='${sid}']`)).toHaveText("alpha");
    await waitForText(owner, SENTENCE);
  } finally {
    await closeAll(owner);
  }
});

test("inbox deep link opens the suggestion", async ({ browser }) => {
  test.setTimeout(120_000);
  const owner = await setupOwner(browser);
  let editor: Page | undefined;
  try {
    await invite(owner, "ed@sg.test", "editor", "ed-hunter22");
    await owner.goto("/");
    const docId = await newDoc(owner);
    await editorOf(owner).click();
    await owner.keyboard.type(SENTENCE);
    await owner.goto("/members");

    editor = await signIn(browser, "ed@sg.test", "ed-hunter22");
    await openDoc(editor, docId);
    await waitForText(editor, SENTENCE);
    await caretToEnd(editor);
    await selectBack(editor, "charlie".length);
    const sid = await suggestReplacement(editor, "delta");

    await owner.reload();
    await expect(owner.getByTestId("inbox-badge")).toHaveText("1", { timeout: 10_000 });
    await owner.getByTestId("sidebar-inbox").click();
    await expect(owner.getByTestId("notifications-dropdown")).toBeVisible();
    await owner.locator("[data-testid='notification-row'][data-kind='suggestion']").first().click();

    await owner.waitForURL(new RegExp(`/doc/${docId}`));
    await connected(owner);
    await expect(owner.getByTestId("sidebar-tab-suggestions")).toHaveAttribute("aria-pressed", "true");
    await expect(owner.getByTestId(`suggestion-card-${sid}`)).toHaveAttribute("data-active", "true", {
      timeout: 10_000,
    });
    await expect.poll(() => owner.url()).not.toContain("suggestion=");

    await expect(owner.getByTestId(`suggestion-card-${sid}`)).toHaveAttribute("data-state", "same", {
      timeout: 10_000,
    });
    // The page text arrives over the collab socket before the accept
    // response, and the author's notification is written just before it.
    const accepted = owner.waitForResponse((r) => r.url().endsWith(`/suggestions/${sid}/accept`));
    await owner.getByTestId(`suggestion-accept-${sid}`).click();
    expect((await accepted).status()).toBe(200);
    await waitForText(owner, "alpha bravo delta");

    await editor.reload();
    await expect(editor.getByTestId("inbox-badge")).toHaveText("1", { timeout: 10_000 });
    await editor.getByTestId("sidebar-inbox").click();
    await expect(
      editor.locator("[data-testid='notification-row'][data-kind='suggestion_decided']"),
    ).toHaveCount(1, { timeout: 10_000 });
  } finally {
    await closeAll(owner, ...(editor ? [editor] : []));
  }
});
