import { expect, test, type Page } from "@playwright/test";

import { docText } from "../support/docText";
import { reset } from "../support/reset";

/**
 * Anchored comments, end to end.
 *
 * `comments.spec.ts` covers the thread lifecycle, but it posts every comment
 * straight to the API with `position_y: null` and never clicks the floating
 * "Add comment" button — so nothing in the suite exercises the anchoring path
 * at all. That path is a Y.RelativePosition encoded through the ySync
 * binding's mapping, and when the binding cannot be reached the encoder simply
 * returns null and the caller persists an empty string. No error, no failed
 * request: the comment saves, the sidebar lists it, and only the highlight is
 * missing.
 *
 * These tests therefore assert the three things that distinguish "anchored"
 * from "saved":
 *   1. the stored `position_y` is a non-empty string,
 *   2. a highlight renders over the anchored words, and
 *   3. the highlight follows the TEXT when someone edits ahead of it —
 *      which a stored byte offset would fail and a relative position passes.
 */

test.beforeAll(reset);

const EMAIL = "owner@anchors.test";
const PASSWORD = "hunter22!anchors";

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

async function newDoc(page: Page): Promise<string> {
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 15_000,
  });
  return page.url().match(/\/doc\/([^/?#]+)/)?.[1] ?? "";
}

const SENTENCE = "alpha bravo charlie";

/** Type the sentence, then select its final word with the keyboard. */
async function typeAndSelectLastWord(page: Page) {
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type(SENTENCE);
  for (let i = 0; i < "charlie".length; i += 1) {
    await page.keyboard.press("Shift+ArrowLeft");
  }
}

/** Anchor a comment on the last word and return the created thread id. */
async function commentOnLastWord(page: Page, body: string) {
  await typeAndSelectLastWord(page);
  await page.getByTestId("add-comment-float").click();
  await expect(page.getByTestId("comment-sidebar")).toBeVisible();
  // The composer only renders when a pending anchor exists, so its presence
  // already proves encodeAnchorRange produced something.
  await page.getByTestId("comment-composer-input-new").fill(body);
  await page.getByTestId("comment-composer-submit-new").click();
}

type CommentRow = {
  id: string;
  thread_id: string;
  parent_id: string | null;
  position_y: string | null;
  position_y_end: string | null;
  anchor_text: string | null;
};

async function listComments(page: Page, docId: string): Promise<CommentRow[]> {
  return page.evaluate(async (id: string) => {
    const r = await fetch(`/api/docs/${id}/comments?include_resolved=true`, {
      credentials: "include",
    });
    return (await r.json()) as CommentRow[];
  }, docId);
}

test("an anchored comment stores a real position and highlights the text", async ({ page }) => {
  await signIn(page);
  const docId = await newDoc(page);

  await commentOnLastWord(page, "why charlie?");

  // 1. The anchor reached the database. An empty string here is the exact
  //    silent failure this spec exists for: the comment still saves.
  await expect
    .poll(async () => (await listComments(page, docId)).length, { timeout: 10_000 })
    .toBeGreaterThan(0);
  const [thread] = await listComments(page, docId);
  expect(thread.anchor_text).toBe("charlie");
  expect(thread.position_y, "position_y was not persisted").toBeTruthy();
  expect(thread.position_y_end, "position_y_end was not persisted").toBeTruthy();

  // 2. The highlight renders, and over the right words.
  const highlight = page.locator("[data-testid='editor-host'] .comment-highlight");
  await expect(highlight).toHaveCount(1);
  await expect(highlight).toHaveText("charlie");
  await expect(highlight).toHaveAttribute("data-comment-id", thread.thread_id);
});

test("the highlight survives a reload", async ({ page }) => {
  await signIn(page);
  await newDoc(page);

  await commentOnLastWord(page, "still here?");
  await expect(page.locator("[data-testid='editor-host'] .comment-highlight")).toHaveText(
    "charlie",
  );

  await page.reload();
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 15_000,
  });

  // Decoding runs against a freshly built binding here, not the one that
  // encoded the anchor — the case a same-session assertion cannot reach.
  await expect(page.locator("[data-testid='editor-host'] .comment-highlight")).toHaveText(
    "charlie",
    { timeout: 10_000 },
  );
});

test("the highlight follows the text when a peer types ahead of it", async ({ page, browser }) => {
  await signIn(page);
  const docUrl = (await newDoc(page), page.url());

  await commentOnLastWord(page, "does this track?");
  await expect(page.locator("[data-testid='editor-host'] .comment-highlight")).toHaveText(
    "charlie",
  );

  // A second session inserts text BEFORE the anchored word. A stored byte
  // offset would now point into "prefix"; a Y.RelativePosition follows the
  // characters it was bound to.
  const peerContext = await browser.newContext();
  try {
    const peer = await peerContext.newPage();
    await signIn(peer);
    await peer.goto(docUrl);
    await expect(peer.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
      timeout: 15_000,
    });
    const peerEditor = peer.locator("[data-testid='editor-host'] .ProseMirror");
    await expect(peerEditor).toContainText(SENTENCE, { timeout: 10_000 });
    // Click the left edge of the first paragraph rather than pressing a
    // "go to start" chord: on macOS Chromium those chords are bound to
    // different editing actions, and the text lands at the caret's old
    // position instead — silently making this a no-op test.
    const firstPara = peerEditor.locator("p").first();
    const box = await firstPara.boundingBox();
    await peer.mouse.click(box!.x + 2, box!.y + box!.height / 2);
    await peer.keyboard.type("PREFIX ");
    await expect
      .poll(() => peerEditor.evaluate(docText), { timeout: 10_000 })
      .toBe(`PREFIX ${SENTENCE}`);
  } finally {
    await peerContext.close();
  }

  // The owner sees the peer's insertion, and the highlight has not slid.
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await expect
    .poll(() => editor.evaluate(docText), { timeout: 10_000 })
    .toBe(`PREFIX ${SENTENCE}`);
  await expect(page.locator("[data-testid='editor-host'] .comment-highlight")).toHaveText(
    "charlie",
    { timeout: 10_000 },
  );
});
