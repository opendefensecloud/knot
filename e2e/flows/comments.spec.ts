import { execSync } from "node:child_process";

import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "comment_reactions", "comments",
    "acl_invalidations", "audit_events", "doc_markdown_cache",
    "doc_snapshots", "doc_updates", "document_grants", "documents",
    "sessions", "workspace_members", "users", "workspaces",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}

test.beforeAll(reset);

test("comments: create thread, reply, react, resolve, show resolved", async ({ page }) => {
  // --- Setup: owner registers + creates a doc ---
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@comments.test");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("hunter22!comments");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  const docId = page.url().match(/\/doc\/([^/?#]+)/)?.[1] ?? "";
  expect(docId).toBeTruthy();

  // Wait for editor to connect
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  // Type some content
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("Hello");
  await page.waitForTimeout(300);

  // --- Open comment sidebar via header button ---
  await page.getByTestId("open-comments").click();
  await expect(page.getByTestId("comment-sidebar")).toBeVisible();

  // No pending anchor yet — the new-thread composer should be absent
  await expect(page.getByTestId("comment-composer-input-new")).not.toBeVisible();

  // --- Post a root thread comment (without anchor) via API fetch ---
  // (Simulates what the composer does; we already captured docId above.)
  const threadRes = await page.evaluate(async (id: string) => {
    const m = document.cookie.match(/(?:^|; )csrf=([^;]*)/);
    const csrf = m && m[1] ? decodeURIComponent(m[1]) : "";
    const r = await fetch(`/api/docs/${id}/comments`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf },
      credentials: "include",
      body: JSON.stringify({ body: "First thread on the doc.", position_y: null, anchor_text: null }),
    });
    return r.json();
  }, docId);

  const threadId = (threadRes as { id?: string }).id ?? "";
  expect(threadId).toBeTruthy();

  // The thread was created via direct fetch (bypassing TanStack Query),
  // so the cached list query is stale. Reload to force a fresh fetch.
  await page.reload();
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
  await page.getByTestId("open-comments").click();
  await expect(page.getByTestId("comment-sidebar")).toBeVisible();

  // Thread should appear
  await expect(page.getByTestId(`comment-thread-${threadId}`)).toBeVisible({ timeout: 8_000 });
  await expect(page.getByTestId(`comment-thread-${threadId}`)).toContainText("First thread on the doc.");

  // --- Reply ---
  const replyInput = page.getByTestId(`comment-composer-input-reply-${threadId}`);
  await replyInput.fill("And a reply");
  await page.getByTestId(`comment-composer-submit-reply-${threadId}`).click();

  // Wait for reply to appear
  await expect(page.getByTestId(`comment-thread-${threadId}`)).toContainText("And a reply", {
    timeout: 6_000,
  });

  // --- Reaction: click 👍 via the add-reaction picker on root comment ---
  // The root comment id equals the thread id (it's the root)
  await page.getByTestId(`comment-react-add-${threadId}`).click();
  // Click 👍 from the emoji picker (the picker renders inline emoji buttons)
  await page.locator(`[data-testid="comment-thread-${threadId}"]`).getByRole("button", { name: "👍" }).click();

  // After adding 👍, the emoji button should appear in the reaction row
  await expect(page.getByTestId(`comment-react-emoji-${threadId}-👍`)).toBeVisible({ timeout: 6_000 });
  await expect(page.getByTestId(`comment-react-emoji-${threadId}-👍`)).toContainText("👍");

  // --- Resolve ---
  await page.getByTestId(`comment-resolve-${threadId}`).click();

  // After resolving, the thread should be gone from the default view (resolved hidden)
  await expect(page.getByTestId(`comment-thread-${threadId}`)).not.toBeVisible({ timeout: 6_000 });

  // --- Show resolved toggle → thread reappears ---
  await page.getByTestId("comment-show-resolved").click();
  await expect(page.getByTestId(`comment-thread-${threadId}`)).toBeVisible({ timeout: 6_000 });
  await expect(page.getByTestId("comment-unresolve-" + threadId)).toBeVisible();

  // --- Close sidebar ---
  await page.getByTestId("comment-sidebar-close").click();
  await expect(page.getByTestId("comment-sidebar")).not.toBeVisible();
});
