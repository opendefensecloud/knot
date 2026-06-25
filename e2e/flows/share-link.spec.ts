import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "share_tokens","acl_invalidations","audit_events","doc_markdown_cache","doc_snapshots",
    "doc_updates","document_grants","documents","sessions","workspace_members","users",
    "workspaces","blobs","blob_bytes",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}
test.beforeAll(reset);

test("owner enables share link; anon reads; owner revokes; anon sees 410", async ({ browser }) => {
  // Owner sets up + creates a doc + opens Permissions, enables Public link.
  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();
  await owner.goto("/setup");
  await owner.getByTestId("setup-email").fill("o@e.com");
  await owner.getByTestId("setup-display-name").fill("O");
  await owner.getByTestId("setup-password").fill("owner-hunter22");
  await owner.getByTestId("setup-submit").click();
  await owner.waitForURL(/\/(?:doc\/.+)?$/);

  await owner.getByTestId("new-doc").click();
  await owner.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await owner.getByTestId("new-doc-blank").click();
  await owner.waitForURL(/\/doc\/.+/);
  const docUrl = owner.url();
  const titleInput = owner.locator("[data-testid='doc-title']");
  await expect(titleInput).toHaveValue("Untitled");
  const patch = owner.waitForResponse(
    (r) => r.url().includes("/api/docs/") && r.request().method() === "PATCH",
  );
  await titleInput.fill("Public Memo");
  await titleInput.blur();
  await patch;

  // Type some content into the doc.
  const editor = owner.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await owner.keyboard.type("Hello world this is public.");
  await owner.waitForTimeout(500);

  // Force a markdown cache fill — the export endpoint renders + caches on
  // demand. Without this, the doc_markdown_cache lags behind the editor
  // until the snapshot policy (200 updates or 30s idle) trips. The public
  // /p/<token> route reads from the cache, so an empty cache → 503.
  // Retry until the export sees the typed content (Yjs WS frames need to
  // reach the room actor before the export can include them).
  const docId = docUrl.match(/\/doc\/([^/?#]+)/)![1]!;
  await expect.poll(
    () =>
      owner.evaluate(async (id) => {
        const r = await fetch(`/api/docs/${id}/markdown`, { credentials: "include" });
        return r.ok ? await r.text() : "";
      }, docId),
    { timeout: 10_000 },
  ).toContain("Hello world");

  // Open Permissions, enable the share link.
  await owner.goto(`${docUrl}/permissions`);
  await expect(owner.getByTestId("permissions-dialog")).toBeVisible();
  await owner.getByTestId("share-enable").click();
  await expect(owner.getByTestId("share-url")).toBeVisible({ timeout: 5_000 });
  const shareUrl = await owner.getByTestId("share-url").inputValue();
  expect(shareUrl).toMatch(/\/p\/[A-Za-z0-9_-]+/);

  // Anon visits the share URL in a fresh context.
  const anonCtx = await browser.newContext();
  const anon = await anonCtx.newPage();
  // Server emits absolute http://localhost:3000/p/... but Vite is on :5173;
  // map the path so the dev proxy handles it.
  const path = shareUrl.replace(/^https?:\/\/[^/]+/, "");
  await anon.goto(path);
  const iframe = anon.frameLocator("iframe[title='Public document']");
  // Doc title is rendered in <title> (browser tab). The actual content
  // ("Hello world this is public.") goes into the article body via
  // pulldown_cmark.
  await expect(iframe.locator("article")).toContainText("Hello world this is public.", {
    timeout: 8_000,
  });

  // Owner revokes. The dialog refreshes; share-enable reappears.
  await owner.getByTestId("share-revoke").click();
  await expect(owner.getByTestId("share-enable")).toBeVisible({ timeout: 5_000 });

  // Anon retries.
  await anon.goto(path);
  await expect(anon.locator("text=Link expired or revoked")).toBeVisible({ timeout: 5_000 });

  await ownerCtx.close();
  await anonCtx.close();
});
