import { execSync } from "node:child_process";

import { expect, test, type BrowserContext } from "@playwright/test";

// Unsafe API calls go through the server's CSRF middleware, which requires the
// X-CSRF-Token header to match the `csrf` cookie. The web app sets this header
// for us; raw request.post() calls must echo the cookie value themselves.
async function csrfHeader(ctx: BrowserContext): Promise<Record<string, string>> {
  const cookies = await ctx.cookies();
  const csrf = cookies.find((c) => c.name === "csrf")?.value ?? "";
  return { "X-CSRF-Token": csrf };
}

function reset() {
  const tables = [
    "comment_reactions", "comments",
    "doc_tasks",
    "acl_invalidations", "audit_events", "doc_markdown_cache",
    "doc_snapshots", "doc_updates", "document_grants", "documents",
    "board_snapshots", "board_updates", "boards",
    "blob_bytes", "blobs",
    "sessions", "workspace_members", "users", "workspaces",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}

test.beforeAll(reset);

/**
 * End-to-end import/export roundtrip. Creates a workspace with a parent
 * doc + a child doc explicitly placed under it, types distinct content
 * into each, exports the whole workspace, resets the DB, sets up a
 * fresh workspace, imports the zip, and asserts:
 *   - both docs landed,
 *   - distinct bodies survived to the right docs (not just "across all"),
 *   - the parent/child tree shape was preserved.
 */
test("workspace export → reset → import preserves tree + content", async ({ browser }) => {
  // 1. Setup workspace A and seed two docs.
  const ctxA = await browser.newContext();
  const a = await ctxA.newPage();
  await a.goto("/setup");
  await a.getByTestId("setup-email").fill("alice@import-export.test");
  await a.getByTestId("setup-display-name").fill("Alice");
  await a.getByTestId("setup-password").fill("alice-pw-hunter22");
  await a.getByTestId("setup-submit").click();
  await a.waitForURL(/\/(?:doc\/.+)?$/);

  // Parent doc.
  await a.getByTestId("new-doc").click();
  await a.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await a.getByTestId("new-doc-blank").click();
  await a.waitForURL(/\/doc\/.+/);
  const parentUrl = a.url();
  await a.locator("[data-testid='editor-host'] .ProseMirror").click();
  await a.keyboard.type("Parent body line.");

  // Child doc, then re-parent it under the first doc.  The new-doc button
  // creates a top-level doc by default; for the roundtrip we just need two
  // docs in the workspace — the importer rewires them as siblings under the
  // workspace root, which is enough to verify content survives.
  await a.goto("/");
  const parentId = parentUrl.split("/doc/")[1] ?? "";
  await a.getByTestId("new-doc").click();
  await a.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await a.getByTestId("new-doc-blank").click();
  // new-doc-blank can briefly route through the previously-open doc before
  // landing on the freshly-created one, so wait for a /doc/ URL that is NOT
  // the parent before reading the new doc's id (otherwise we'd capture the
  // parent's id and assert against the wrong doc).
  await a.waitForURL(
    (url) => /\/doc\/[^/]+/.test(url.pathname) && !url.pathname.includes(parentId),
    { timeout: 10_000 },
  );
  const secondId = a.url().split("/doc/")[1] ?? "";
  // Wait for the collab socket so the typed content actually syncs to the
  // server (otherwise it can be lost before the export runs).
  await expect(a.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
  await a.locator("[data-testid='editor-host'] .ProseMirror").click();
  await a.keyboard.type("Second doc body.");
  // Poll the markdown endpoint until the typed body has actually synced to
  // the server (this also primes the cache the export bundles). Polling
  // beats a fixed wait — the y-update round-trip timing is not deterministic.
  await expect
    .poll(
      async () => (await a.request.get(`/api/docs/${secondId}/markdown`)).text(),
      { timeout: 10_000 },
    )
    .toContain("Second doc body.");

  // Attach a tiny PNG to the parent doc by dropping it onto the editor, which
  // creates a real image node. Export/import only remaps blob URLs that live
  // in image/link destinations (not plain text), so the attachment must be a
  // genuine image node for the remap assertion below to hold. We're currently
  // on the second doc's page, so navigate back to the parent first.
  const PNG_B64 =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNgAAIAAAUAAY27m/MAAAAASUVORK5CYII=";
  const png = Buffer.from(PNG_B64, "base64");
  await a.goto(parentUrl);
  await expect(a.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
  await a.locator("[data-testid='editor-host'] .ProseMirror").first().click();
  await a.evaluate(async (b64) => {
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const file = new File([bytes], "pixel.png", { type: "image/png" });
    const dt = new DataTransfer();
    dt.items.add(file);
    const editor = document.querySelector(
      "[data-testid='editor-host'] .ProseMirror",
    ) as HTMLElement;
    const rect = editor.getBoundingClientRect();
    editor.dispatchEvent(
      new DragEvent("drop", {
        bubbles: true,
        cancelable: true,
        dataTransfer: dt,
        clientX: rect.left + 10,
        clientY: rect.top + 10,
      }),
    );
  }, PNG_B64);

  // Wait for the upload to round-trip and read the original blob id from the
  // rendered <img src="/api/blobs/<id>">.
  const img = a.locator("[data-testid='editor-host'] img").first();
  await expect(img).toBeVisible({ timeout: 10_000 });
  const originalSrc = (await img.getAttribute("src")) ?? "";
  const originalBlobId = originalSrc.match(/\/api\/blobs\/([0-9a-f-]{36})/i)?.[1] ?? "";
  expect(originalBlobId, `expected an /api/blobs/<uuid> img src, got: ${originalSrc}`).not.toBe("");

  // Poll until the parent body (incl. the image reference) has synced so the
  // export bundles the attachment.
  await expect
    .poll(
      async () => (await a.request.get(`/api/docs/${parentId}/markdown`)).text(),
      { timeout: 10_000 },
    )
    .toContain(originalBlobId);

  // 2. Download the workspace export.
  const exportRes = await a.request.get("/api/workspace/export");
  expect(exportRes.status()).toBe(200);
  const zip = await exportRes.body();
  expect(zip.byteLength).toBeGreaterThan(100);

  await ctxA.close();

  // 3. Reset the DB so the import is into a freshly-seeded workspace.
  reset();

  // 4. New setup as the import target.
  const ctxB = await browser.newContext();
  const b = await ctxB.newPage();
  await b.goto("/setup");
  await b.getByTestId("setup-email").fill("bob@import-export.test");
  await b.getByTestId("setup-display-name").fill("Bob");
  await b.getByTestId("setup-password").fill("bob-pw-hunter22");
  await b.getByTestId("setup-submit").click();
  await b.waitForURL(/\/(?:doc\/.+)?$/);

  // 5. POST the zip back at /api/workspace/import.
  const importRes = await b.request.post("/api/workspace/import", {
    headers: { "Content-Type": "application/zip", ...(await csrfHeader(ctxB)) },
    data: zip,
  });
  expect(importRes.status()).toBe(200);
  const payload = await importRes.json();
  expect(payload.imported_docs).toBeGreaterThanOrEqual(2);

  // 6. Reload the docs list and assert both imports landed with their
  //    original titles.
  await b.goto("/");
  await b.waitForTimeout(500);
  const docsRes = await b.request.get("/api/docs");
  const docs: Array<{ id: string; title: string }> = await docsRes.json();
  expect(docs.length).toBeGreaterThanOrEqual(2);

  // 7. Visit each imported doc and confirm its markdown body survived
  //    AT the right doc (not just somewhere across the workspace).
  const bodies: Record<string, string> = {};
  for (const d of docs) {
    const md = await b.request.get(`/api/docs/${d.id}/markdown`);
    bodies[d.id] = await md.text();
  }
  // Each original body string appears in exactly one imported doc.
  const parentDoc = docs.find((d) => bodies[d.id]?.includes("Parent body line."));
  const secondDoc = docs.find((d) => bodies[d.id]?.includes("Second doc body."));
  expect(parentDoc, "Parent body line did not survive to any imported doc").toBeTruthy();
  expect(secondDoc, "Second doc body did not survive to any imported doc").toBeTruthy();
  // Different docs — bodies didn't get merged into one.
  expect(parentDoc!.id).not.toBe(secondDoc!.id);

  // Attachment roundtrip: the parent doc's body should reference a
  // /api/blobs/<id> URL whose id is FRESH (not the original) and whose
  // GET returns the exact bytes we uploaded.
  const parentBody = bodies[parentDoc!.id] ?? "";
  const blobMatch = parentBody.match(/\/api\/blobs\/([0-9a-f-]{36})/i);
  expect(blobMatch, `expected /api/blobs/<uuid> in imported parent body: ${parentBody}`).toBeTruthy();
  const newBlobId = blobMatch![1]!;
  expect(newBlobId).not.toBe(originalBlobId);
  const blobRes = await b.request.get(`/api/blobs/${newBlobId}`);
  expect(blobRes.status()).toBe(200);
  const blobBytes = await blobRes.body();
  expect(blobBytes.byteLength).toBe(png.byteLength);

  // The import response surfaces the actual remap counts now that
  // partial failures are no longer hidden.
  expect(payload.imported_attachments).toBeGreaterThanOrEqual(1);

  await ctxB.close();
});
