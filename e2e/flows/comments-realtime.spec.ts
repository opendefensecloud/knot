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

/**
 * Acceptance proof for the realtime-comments feature:
 *
 * Pipeline: POST /api/docs/:id/comments
 *   → pg_notify 'doc_comments'
 *   → backend LISTEN/NOTIFY listener
 *   → MSG_COMMENTS frame forwarded to all collab WS clients in the doc's room
 *   → KnotProvider "comments" event fires on every connected client
 *   → KnotEditor invalidates ["comments", docId] in React Query
 *   → CommentSidebar refetches and renders the new comment
 *
 * This test asserts that when the owner posts a comment (and a reply), the
 * editor (second browser context) sees both appear in their sidebar WITHOUT
 * reloading.
 */
test(
  "owner posts comment + reply; editor sees both in realtime without reload",
  async ({ browser }) => {
    test.setTimeout(90_000);

    const COMMENT_TEXT = "live-comment-rt-1749600000";
    const REPLY_TEXT = "live-reply-rt-1749600000";

    // -----------------------------------------------------------------------
    // Owner: setup workspace + create a doc
    // -----------------------------------------------------------------------
    const ownerCtx = await browser.newContext();
    const owner = await ownerCtx.newPage();

    await owner.goto("/setup");
    await owner.getByTestId("setup-email").fill("owner@rt.test");
    await owner.getByTestId("setup-display-name").fill("Owner");
    await owner.getByTestId("setup-password").fill("owner-hunter22");
    await owner.getByTestId("setup-submit").click();
    await owner.waitForURL(/\/(?:doc\/.+)?$/);

    await owner.getByTestId("new-doc").click();
    // The "New document" picker modal appears; click "Blank document".
    await owner.getByTestId("new-doc-blank").click();
    await owner.waitForURL(/\/doc\/.+/);
    const docUrl = owner.url();
    const docId = docUrl.match(/\/doc\/([^/?#]+)/)?.[1] ?? "";
    expect(docId).toBeTruthy();

    // Wait for owner to be connected to the collab room (required so the WS
    // listener is active and will relay MSG_COMMENTS to the editor peer).
    await expect(owner.getByTestId("status-dot")).toHaveAttribute(
      "data-status",
      "connected",
      { timeout: 10_000 },
    );

    // -----------------------------------------------------------------------
    // Owner: invite editor (Bob) with a password
    // -----------------------------------------------------------------------
    await owner.goto("/members");
    await owner.getByTestId("invite-email").fill("bob@rt.test");
    await owner.getByTestId("invite-role").selectOption("editor");
    await owner.getByTestId("invite-password").fill("bob-hunter22");
    await owner.getByTestId("invite-submit").click();
    await expect(owner.locator("[data-testid^='member-']")).toHaveCount(2, {
      timeout: 5_000,
    });

    // -----------------------------------------------------------------------
    // Editor (Bob): sign in, navigate to the SAME doc in editor view,
    // open the comment sidebar — then STAY on the page.
    // -----------------------------------------------------------------------
    const bobCtx = await browser.newContext();
    const bob = await bobCtx.newPage();

    await bob.goto("/login");
    await bob.getByTestId("login-email").fill("bob@rt.test");
    await bob.getByTestId("login-password").fill("bob-hunter22");
    await bob.getByTestId("login-submit").click();
    await bob.waitForURL(/\/(?:doc\/.+)?$/, { timeout: 10_000 });

    // Navigate to the same doc so Bob connects to the collab room.
    await bob.goto(docUrl);
    await expect(bob.getByTestId("status-dot")).toHaveAttribute(
      "data-status",
      "connected",
      { timeout: 10_000 },
    );

    // Open Bob's comment sidebar.
    await bob.getByTestId("open-comments").click();
    await expect(bob.getByTestId("comment-sidebar")).toBeVisible();

    // -----------------------------------------------------------------------
    // Owner: navigate back to the doc, reconnect to the collab room,
    // open the comment sidebar.
    // -----------------------------------------------------------------------
    await owner.goto(docUrl);
    await expect(owner.getByTestId("status-dot")).toHaveAttribute(
      "data-status",
      "connected",
      { timeout: 10_000 },
    );
    await owner.getByTestId("open-comments").click();
    await expect(owner.getByTestId("comment-sidebar")).toBeVisible();

    // -----------------------------------------------------------------------
    // Owner: POST a new comment thread via the API.
    // This hits the REST endpoint that triggers pg_notify → WS frame → Bob.
    // -----------------------------------------------------------------------
    const threadRes = await owner.evaluate(
      async ([id, body]: [string, string]) => {
        const m = document.cookie.match(/(?:^|; )csrf=([^;]*)/);
        const csrf = m && m[1] ? decodeURIComponent(m[1]) : "";
        const r = await fetch(`/api/docs/${id}/comments`, {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            "X-CSRF-Token": csrf,
          },
          credentials: "include",
          body: JSON.stringify({
            body,
            position_y: null,
            position_y_end: null,
            anchor_text: null,
          }),
        });
        return r.json() as Promise<{ id?: string; error?: string }>;
      },
      [docId, COMMENT_TEXT] as [string, string],
    );

    const threadId = (threadRes as { id?: string }).id ?? "";
    expect(threadId).toBeTruthy();

    // -----------------------------------------------------------------------
    // ASSERT (primary): Bob sees the new comment appear WITHOUT reloading.
    // The pipeline is: pg_notify → WS MSG_COMMENTS → React Query invalidate
    // → sidebar refetch. Allow 10 s for the round-trip.
    // -----------------------------------------------------------------------
    await expect(bob.getByText(COMMENT_TEXT)).toBeVisible({ timeout: 10_000 });

    // -----------------------------------------------------------------------
    // Owner: post a REPLY on the same thread.
    // -----------------------------------------------------------------------
    const replyRes = await owner.evaluate(
      async ([id, tid, body]: [string, string, string]) => {
        const m = document.cookie.match(/(?:^|; )csrf=([^;]*)/);
        const csrf = m && m[1] ? decodeURIComponent(m[1]) : "";
        const r = await fetch(
          `/api/docs/${id}/comments/${tid}/replies`,
          {
            method: "POST",
            headers: {
              "Content-Type": "application/json",
              "X-CSRF-Token": csrf,
            },
            credentials: "include",
            body: JSON.stringify({ body }),
          },
        );
        return r.json() as Promise<{ id?: string; error?: string }>;
      },
      [docId, threadId, REPLY_TEXT] as [string, string, string],
    );

    expect((replyRes as { id?: string }).id).toBeTruthy();

    // -----------------------------------------------------------------------
    // ASSERT (reply direction): Bob sees the reply appear WITHOUT reloading.
    // -----------------------------------------------------------------------
    await expect(bob.getByText(REPLY_TEXT)).toBeVisible({ timeout: 10_000 });

    await ownerCtx.close();
    await bobCtx.close();
  },
);
