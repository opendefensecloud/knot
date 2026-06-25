import { execSync } from "node:child_process";

import { expect, test, type Page } from "@playwright/test";

function reset() {
  const tables = [
    "board_snapshots", "board_updates", "boards",
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
 * Drive a rectangle into the open Excalidraw modal via the dev-only
 * `window.__excalidrawAPI` + `window.__excalidrawConvert` hooks set by
 * ExcalidrawModal.tsx. Returns the id of the inserted element.
 */
async function drawRectangle(
  page: Page,
  opts: { x: number; y: number; width: number; height: number; strokeColor?: string },
): Promise<string> {
  return await page.evaluate(({ x, y, width, height, strokeColor }) => {
    const w = window as unknown as {
      __excalidrawAPI?: {
        getSceneElements: () => readonly { id: string }[];
        updateScene: (s: { elements: readonly unknown[] }) => void;
      };
      __excalidrawConvert?: (
        skeletons: ReadonlyArray<Record<string, unknown>>,
      ) => readonly { id: string }[];
    };
    const api = w.__excalidrawAPI;
    const convert = w.__excalidrawConvert;
    if (!api || !convert) throw new Error("excalidraw test hooks not ready");
    const existing = api.getSceneElements();
    const built = convert([
      { type: "rectangle", x, y, width, height, strokeColor: strokeColor ?? "#1e1e1e" },
    ]);
    const next = [...existing, ...built];
    api.updateScene({ elements: next });
    return built[0].id;
  }, opts);
}

async function waitForExcalidrawReady(page: Page): Promise<void> {
  await expect(page.getByTestId("excalidraw-modal")).toBeVisible({ timeout: 10_000 });
  // Wait for the lazy Excalidraw chunk + handleApi callback to publish hooks.
  await page.waitForFunction(
    () => {
      const w = window as unknown as {
        __excalidrawAPI?: unknown;
        __excalidrawConvert?: unknown;
      };
      return !!w.__excalidrawAPI && !!w.__excalidrawConvert;
    },
    null,
    { timeout: 15_000 },
  );
}

test("excalidraw: insert board, draw rectangle, inline preview renders", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@excalidraw.test");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("hunter22!excalidraw");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);

  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();

  // Insert a board via toolbar.
  await page.getByTestId("toolbar-excalidraw").click();

  // Inline NodeView appears.
  await expect(page.getByTestId("excalidraw-board").first()).toBeVisible({ timeout: 8_000 });

  // Open the modal.
  await page.getByTestId("excalidraw-board-open").first().click();
  await waitForExcalidrawReady(page);

  // Draw a rectangle programmatically.
  await drawRectangle(page, { x: 100, y: 100, width: 120, height: 80 });

  // Close the modal — this triggers a fire-and-forget SVG snapshot save.
  await page.getByTestId("excalidraw-modal-close").click();
  await expect(page.getByTestId("excalidraw-modal")).toHaveCount(0);

  // The inline preview re-fetches the SVG (react-query invalidation on save).
  // Wait until the inline NodeView shows an inline SVG element.
  await expect(
    page.locator("[data-testid='excalidraw-board'] svg").first(),
  ).toBeVisible({ timeout: 10_000 });
});

test("excalidraw: two contexts converge on a shared board", async ({ browser }) => {
  // Alice sets up, creates a doc, inserts a board, invites Bob.
  const aliceCtx = await browser.newContext();
  const alice = await aliceCtx.newPage();
  await alice.goto("/setup");
  await alice.getByTestId("setup-email").fill("alice@excalidraw.test");
  await alice.getByTestId("setup-display-name").fill("Alice");
  await alice.getByTestId("setup-password").fill("alice-hunter22");
  await alice.getByTestId("setup-submit").click();
  await alice.getByTestId("new-doc").click();
  await alice.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await alice.getByTestId("new-doc-blank").click();
  await alice.waitForURL(/\/doc\/.+/);
  const docUrl = alice.url();

  await expect(alice.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
  const aliceEditor = alice.locator("[data-testid='editor-host'] .ProseMirror");
  await aliceEditor.click();
  await alice.getByTestId("toolbar-excalidraw").click();
  await expect(alice.getByTestId("excalidraw-board").first()).toBeVisible({ timeout: 8_000 });

  await alice.goto("/members");
  await alice.getByTestId("invite-email").fill("bob@excalidraw.test");
  await alice.getByTestId("invite-role").selectOption("editor");
  await alice.getByTestId("invite-password").fill("bob-hunter22");
  await alice.getByTestId("invite-submit").click();
  await expect(alice.locator("[data-testid^='member-']")).toHaveCount(2, { timeout: 5_000 });

  // Bob signs in in a separate context.
  const bobCtx = await browser.newContext();
  const bob = await bobCtx.newPage();
  await bob.goto("/login");
  await bob.getByTestId("login-email").fill("bob@excalidraw.test");
  await bob.getByTestId("login-password").fill("bob-hunter22");
  await bob.getByTestId("login-submit").click();
  await bob.waitForURL(/\/(?:doc\/.+)?$/, { timeout: 5_000 });

  // Both back to the doc.
  await alice.goto(docUrl);
  await bob.goto(docUrl);

  await expect(alice.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });
  await expect(bob.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  await expect(alice.getByTestId("excalidraw-board").first()).toBeVisible({ timeout: 8_000 });
  await expect(bob.getByTestId("excalidraw-board").first()).toBeVisible({ timeout: 8_000 });

  // Alice opens the board and draws a rectangle.
  await alice.getByTestId("excalidraw-board-open").first().click();
  await waitForExcalidrawReady(alice);
  await drawRectangle(alice, { x: 100, y: 100, width: 120, height: 80, strokeColor: "#aa0000" });

  // Bob opens the same board.
  await bob.getByTestId("excalidraw-board-open").first().click();
  await waitForExcalidrawReady(bob);

  // Bob should see Alice's rectangle propagate via the BoardProvider WS.
  const countElements = (page: Page) =>
    page.evaluate(() => {
      const w = window as unknown as {
        __excalidrawAPI?: { getSceneElements: () => readonly unknown[] };
      };
      return w.__excalidrawAPI?.getSceneElements().length ?? 0;
    });
  await expect.poll(() => countElements(bob), { timeout: 10_000 }).toBeGreaterThanOrEqual(1);

  // Bob draws a second rectangle.
  await drawRectangle(bob, { x: 300, y: 100, width: 120, height: 80, strokeColor: "#0000aa" });

  // Both contexts should converge on two elements.
  await expect.poll(() => countElements(alice), { timeout: 10_000 }).toBeGreaterThanOrEqual(2);
  await expect.poll(() => countElements(bob), { timeout: 10_000 }).toBeGreaterThanOrEqual(2);

  await aliceCtx.close();
  await bobCtx.close();
});

test("excalidraw: markdown export references board sentinel", async ({ page, request }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("owner@excalidraw-md.test");
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill("hunter22!exmd");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  const url = new URL(page.url());
  const docId = url.pathname.replace(/^\/doc\//, "");

  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.getByTestId("toolbar-excalidraw").click();
  await expect(page.getByTestId("excalidraw-board").first()).toBeVisible({ timeout: 8_000 });

  // Give the inserted Y update time to flush to the server, then export.
  // Poll the markdown endpoint until the board sentinel appears.
  const cookies = await page.context().cookies();
  const cookieHeader = cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  await expect
    .poll(
      async () => {
        const res = await request.get(`http://localhost:3000/api/docs/${docId}/markdown`, {
          headers: { cookie: cookieHeader },
        });
        if (!res.ok()) return "";
        return await res.text();
      },
      { timeout: 15_000 },
    )
    .toMatch(/knot:\/\/board\/[0-9a-f-]{36}\.svg/);
});
