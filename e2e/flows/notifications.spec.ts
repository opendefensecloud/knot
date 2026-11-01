import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);

/**
 * Acceptance proof for the inbox:
 *
 *   owner comments, mentioning an editor picked from the mention picker
 *     → a notifications row is written for that editor
 *     → their badge shows 1
 *     → the dropdown lists it; clicking lands on the doc with the thread open
 *     → the badge clears
 *   a thread resolved before its notification is opened still opens
 *
 * The mentioned member's display name contains a space, which the server's
 * @(\w+) fallback can never resolve — so this also proves the picker is
 * sending user ids.
 */
test("a mention reaches the mentioned user's inbox", async ({ browser }) => {
  test.setTimeout(120_000);

  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();

  await owner.goto("/setup");
  await owner.getByTestId("setup-email").fill("owner@inbox.test");
  await owner.getByTestId("setup-display-name").fill("Owner");
  await owner.getByTestId("setup-password").fill("owner-hunter22");
  await owner.getByTestId("setup-submit").click();
  await owner.waitForURL(/\/(?:doc\/.+)?$/);

  // An editor whose display name contains a space.
  await owner.goto("/members");
  await owner.getByTestId("invite-email").fill("mara@inbox.test");
  await owner.getByTestId("invite-display-name").fill("Mara Jade");
  await owner.getByTestId("invite-role").selectOption("editor");
  await owner.getByTestId("invite-password").fill("mara-hunter22");
  await owner.getByTestId("invite-submit").click();
  // `owner.goto` below is a hard navigation, which would abort the invite
  // mutation's in-flight fetch if it hasn't landed yet — wait for the
  // members table to reflect it first.
  await expect(owner.locator("[data-testid^='member-']")).toHaveCount(2, { timeout: 5_000 });

  await owner.goto("/");
  await owner.getByTestId("new-doc").click();
  await owner.getByTestId("new-doc-blank").click();
  await owner.waitForURL(/\/doc\/.+/);
  const docId = owner.url().match(/\/doc\/([^/?#]+)/)?.[1] ?? "";
  expect(docId).toBeTruthy();
  await expect(owner.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 30_000,
  });

  // The new-thread composer only renders once a pending anchor exists
  // (CommentSidebar.tsx), which requires selecting text in the editor and
  // clicking the floating "Add comment" button — it opens the sidebar
  // itself, so no separate `open-comments` click is needed here.
  const editor = owner.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await owner.keyboard.type("please review this");
  for (let i = 0; i < "this".length; i++) {
    await owner.keyboard.press("Shift+ArrowLeft");
  }
  await owner.getByTestId("add-comment-float").click();
  await expect(owner.getByTestId("comment-sidebar")).toBeVisible();

  // Comment mentioning Mara, picked from the picker so her id is sent.
  const input = owner.getByTestId("comment-composer-input-new");
  await input.click();
  // pressSequentially fires real key events; the picker opens on keyup.
  await input.pressSequentially("please review @Mara");
  await owner.getByTestId("mention-item").first().click();
  await owner.getByTestId("comment-composer-submit-new").click();

  // Mara logs in and finds it waiting.
  const maraCtx = await browser.newContext();
  const mara = await maraCtx.newPage();
  await mara.goto("/login");
  await mara.getByTestId("login-email").fill("mara@inbox.test");
  await mara.getByTestId("login-password").fill("mara-hunter22");
  await mara.getByTestId("login-submit").click();
  await mara.waitForURL(/\/(?:doc\/.+)?$/);

  await expect(mara.getByTestId("inbox-badge")).toHaveText("1");

  await mara.getByTestId("sidebar-inbox").click();
  await expect(mara.getByTestId("notifications-dropdown")).toBeVisible();
  const row = mara.getByTestId("notification-row").first();
  await expect(row).toHaveAttribute("data-kind", "mention");
  await row.click();

  await mara.waitForURL(new RegExp(`/doc/${docId}`));
  await expect(mara.getByTestId("comment-sidebar")).toBeVisible();
  const activeThread = mara.locator("[data-testid^='comment-thread-'][data-active='true']");
  await expect(activeThread).toBeVisible();
  // The link's ?thread= is dropped once used.
  await expect(mara).not.toHaveURL(/thread=/);
  await expect(mara.getByTestId("inbox-badge")).toHaveCount(0);

  // A second thread mentioning Mara, resolved before she opens it.
  await editor.click();
  await owner.keyboard.press("End");
  await owner.keyboard.type(" and that");
  for (let i = 0; i < "that".length; i++) {
    await owner.keyboard.press("Shift+ArrowLeft");
  }
  await owner.getByTestId("add-comment-float").click();
  const second = owner.getByTestId("comment-composer-input-new");
  await second.click();
  await second.pressSequentially("this too @Mara");
  await owner.getByTestId("mention-item").first().click();
  await owner.getByTestId("comment-composer-submit-new").click();
  const resolveButtons = owner.locator("[data-testid^='comment-resolve-']");
  // Both threads listed, the new one last.
  await expect(resolveButtons).toHaveCount(2);
  const resolve = resolveButtons.last();
  const resolvedThreadId = ((await resolve.getAttribute("data-testid")) ?? "").replace(
    "comment-resolve-",
    "",
  );
  expect(resolvedThreadId).toBeTruthy();
  await resolve.click();

  await mara.goto("/");
  await mara.getByTestId("sidebar-inbox").click();
  await mara.getByTestId("notification-row").first().click();
  await mara.waitForURL(new RegExp(`/doc/${docId}`));
  await expect(mara.getByTestId(`comment-thread-${resolvedThreadId}`)).toHaveAttribute(
    "data-active",
    "true",
  );

  // The full page renders too.
  await mara.goto("/notifications");
  await expect(mara.getByTestId("notifications-page")).toBeVisible();
  await expect(mara.getByTestId("notification-row").first()).toBeVisible();

  await ownerCtx.close();
  await maraCtx.close();
});
