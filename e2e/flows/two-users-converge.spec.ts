import { execSync } from "node:child_process";

import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
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
 * Walk only bare Text nodes inside a ProseMirror element, skipping any
 * collaboration-cursor label spans so cursor decorations don't pollute the
 * result.
 */
function docText(el: Element): string {
  const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT, {
    acceptNode(node) {
      let p: Node | null = node.parentElement;
      while (p && p !== el) {
        if (p instanceof Element && p.classList.contains("collaboration-cursor__label")) {
          return NodeFilter.FILTER_REJECT;
        }
        p = p.parentNode;
      }
      return NodeFilter.FILTER_ACCEPT;
    },
  });
  const parts: string[] = [];
  let n: Node | null;
  while ((n = walker.nextNode())) parts.push(n.textContent ?? "");
  return parts.join("");
}

test("two users editing concurrently converge on both screens", async ({ browser }) => {
  // Alice sets up the workspace + creates a doc + invites Bob with password.
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

  await alice.goto("/members");
  await alice.getByTestId("invite-email").fill("bob@example.com");
  await alice.getByTestId("invite-role").selectOption("editor");
  await alice.getByTestId("invite-password").fill("bob-hunter22");
  await alice.getByTestId("invite-submit").click();
  // Wait for Bob to appear in the members table (any member-* testid besides Alice's).
  await expect(alice.locator("[data-testid^='member-']")).toHaveCount(2, { timeout: 5_000 });

  // Bob signs in in a separate browser context.
  const bobCtx = await browser.newContext();
  const bob = await bobCtx.newPage();
  await bob.goto("/login");
  await bob.getByTestId("login-email").fill("bob@example.com");
  await bob.getByTestId("login-password").fill("bob-hunter22");
  await bob.getByTestId("login-submit").click();
  await bob.waitForURL(/\/(?:doc\/.+)?$/, { timeout: 5_000 });

  // Both navigate to the doc.
  await alice.goto(docUrl);
  await bob.goto(docUrl);

  // Both reach connected.
  await expect(alice.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });
  await expect(bob.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  // Type from each side.
  const aliceEditor = alice.locator("[data-testid='editor-host'] .ProseMirror");
  const bobEditor = bob.locator("[data-testid='editor-host'] .ProseMirror");

  await aliceEditor.click();
  await alice.keyboard.type("Hello from Alice. ");
  // Wait for Alice's text to propagate to Bob's editor before Bob starts typing,
  // so their edits land at different positions and the CRDT merge is unambiguous.
  await expect.poll(() => bobEditor.evaluate(docText), { timeout: 8_000 }).toMatch(/Hello from Alice\./);

  // Click the editor, move cursor to end of line, then type Bob's contribution.
  // The 200ms pause after End lets ProseMirror settle its cursor state.
  await bobEditor.click();
  await bob.keyboard.press("End");
  await bob.waitForTimeout(200);
  await bob.keyboard.type("And from Bob.");

  // Both screens see both contributions within the poll window.
  // Poll Bob's editor first so we know his Yjs doc has the text before checking Alice.
  await expect.poll(() => bobEditor.evaluate(docText), { timeout: 5_000 }).toMatch(/Hello from Alice\./);
  await expect.poll(() => bobEditor.evaluate(docText), { timeout: 5_000 }).toMatch(/And from Bob\./);
  await expect.poll(() => aliceEditor.evaluate(docText), { timeout: 5_000 }).toMatch(/Hello from Alice\./);
  await expect.poll(() => aliceEditor.evaluate(docText), { timeout: 5_000 }).toMatch(/And from Bob\./);

  await aliceCtx.close();
  await bobCtx.close();
});
